// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Serving a request: its buffers ([`Buffers`]) read, the cell asked ([`Backend`]), and what it
//! returns and the response written back, as doc-4 has it. One interpreter for every transport
//! that carries doc-4's requests (board 14378): a guest's extent device, its VMM serving its
//! virtqueues, and a local process's ring to the extent daemon, the daemon serving it. What
//! differs between them is a [`Dialect`]: the flags each request type takes, and the types only
//! one of them serves.

use crate::{Errno, MappingContents, WindowMapping};
use log::error;

use super::backend::Backend;
use super::buffers::{Buffers, Reader, Writer};
use super::protocol::*;

/// What a transport's requests are: which types it serves and the flags each takes, and what it
/// does with its own. A type parameter of [`handle`], so that each transport's interpreter is
/// compiled apart, with nothing chosen at run time: the guest's device's
/// ([`super::Guest`], doc-4 as it is) and a local process's ring's ([`Process`]).
pub trait Dialect<S: Backend>: Sync {
    /// The bit of a `COMMIT`'s flags that has it complete once local, synced to the host's disk
    /// ([`crate::Commit::Local`]), if the dialect has one.
    const COMMIT_LOCAL: Option<u32>;
    /// The bit of a `WRITE`'s flags that has its commit complete once local, if it has one.
    const WRITE_LOCAL: Option<u32>;
    /// The bit of a `MAP`'s flags that has the daemon warm each mapped range in its extent cache
    /// first (board 11681), if it has one.
    const MAP_WARM: Option<u32>;

    /// The flags requests of `type_` may set: `None` for a type the dialect serves none of. A
    /// request setting any other is refused (`EINVAL`) before anything of it is served, in one
    /// place for every type (board 2774).
    fn known_flags(&self, type_: u32) -> Option<u32>;

    /// A request of `type_`'s `flags`, known, as they are served.
    fn served_flags(&self, type_: u32, flags: u32) -> u32;

    /// `MAP_PREFAULT`: has `len` bytes of the window at `window_offset` mapped ahead of the
    /// accesses that come next, once mapped. It must not wait.
    fn prefault(&self, cell: &S, window_offset: u64, len: u64);

    /// Serves a request of `type_`, one of the dialect's own (no `READ`, `WRITE`, `FREE`, `MAP`,
    /// `UNMAP`, `COMMIT`, `ROOT` nor `INFO`), its header read from `reader` and its `flags` known,
    /// as [`handle`] serves the others, within `bounds`: what it returns written with `writer`,
    /// which has `room` bytes for it, and how many it wrote.
    #[allow(clippy::too_many_arguments, reason = "a request's parts, as `serve` has them")]
    fn serve_own(
        &self,
        cell: &S,
        type_: u32,
        flags: u32,
        bounds: Bounds,
        reader: &mut Reader<'_>,
        writer: &Writer<'_>,
        room: usize,
    ) -> impl Future<Output = Result<usize, Errno>> + Send;
}

/// A local process's requests, on its ring to the extent daemon (board 14378): doc-4's data
/// requests, `READ`, `WRITE`, `FREE`, `MAP`, `UNMAP`, `COMMIT`, `ROOT` and `INFO`, with the flags of
/// a cell kept outside a VM besides ([`EXTENT_RING_COMMIT_LOCAL`], [`EXTENT_RING_WRITE_LOCAL`],
/// [`EXTENT_RING_MAP_WARM`]). What a guest's alone ask, its VMM's (`MAP_PREFAULT`, `PRIVATE`,
/// `FREEZE`) and its answers to the daemon (`STATS`, `RESIZED`), it doesn't serve: the process
/// does the first itself, and says the others on its socket.
#[derive(Clone, Copy, Debug, Default)]
pub struct Process;

impl<S: Backend> Dialect<S> for Process {
    const COMMIT_LOCAL: Option<u32> = Some(EXTENT_RING_COMMIT_LOCAL);
    const WRITE_LOCAL: Option<u32> = Some(EXTENT_RING_WRITE_LOCAL);
    const MAP_WARM: Option<u32> = Some(EXTENT_RING_MAP_WARM);

    fn known_flags(&self, type_: u32) -> Option<u32> {
        Some(match type_ {
            VIRTIO_EXTENT_T_WRITE => crate::protocol::WRITE_SHARED_FLAGS | EXTENT_RING_WRITE_LOCAL,
            VIRTIO_EXTENT_T_FREE => VIRTIO_EXTENT_FREE_MANY,
            VIRTIO_EXTENT_T_MAP => {
                VIRTIO_EXTENT_MAP_MANY | VIRTIO_EXTENT_MAP_ZEROS | EXTENT_RING_MAP_WARM
            }
            VIRTIO_EXTENT_T_COMMIT => {
                crate::protocol::COMMIT_SHARED_FLAGS | EXTENT_RING_COMMIT_LOCAL
            }
            VIRTIO_EXTENT_T_READ
            | VIRTIO_EXTENT_T_UNMAP
            | VIRTIO_EXTENT_T_ROOT
            | VIRTIO_EXTENT_T_INFO => 0,
            _ => return None,
        })
    }

    fn served_flags(&self, _: u32, flags: u32) -> u32 {
        flags
    }

    // `MAP_PREFAULT` is no flag of a process's (`known_flags`): never asked.
    fn prefault(&self, _: &S, _: u64, _: u64) {}

    // Its every type is one of the common ones (`known_flags`): never asked.
    async fn serve_own(
        &self,
        _: &S,
        _: u32,
        _: u32,
        _: Bounds,
        _: &mut Reader<'_>,
        _: &Writer<'_>,
        _: usize,
    ) -> Result<usize, Errno> {
        Err(Errno::EINVAL)
    }
}

/// What serving a request came to ([`handle`]): the bytes written to it, its writable buffers'
/// whole, whose last bytes hold the response, which its transport returns it to its driver with;
/// and the response's status, which the transport counts it by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Handled {
    pub len: u32,
    pub status: Result<(), Errno>,
}

/// Serves the request whose buffers are `buffers` with `cell`, as `dialect` has its requests,
/// refusing what is out of `bounds` before `cell` sees it: what was written to it, and its status.
/// One with no room for its response is returned with nothing written, its length 0.
pub async fn handle<S: Backend, D: Dialect<S>>(
    cell: &S,
    dialect: &D,
    bounds: Bounds,
    buffers: Buffers,
) -> Handled {
    // The response is the chain's last bytes, after what the request returns.
    let writable = buffers.writable_len();
    let Some(room) = writable.checked_sub(size_of::<Response>()) else {
        error!("extent requests: a request has no room for its response");
        return Handled { len: 0, status: Err(Errno::EINVAL) };
    };
    let len = u32::try_from(writable).expect("a request's buffers' writable length fits in u32");
    let mut reader = Reader::new(&buffers);
    let writer = Writer::new(&buffers);
    // cladefs: more room than any request returns into, refused, its response alone written: none
    // of it zeroed, which a chain naming 4 GiB had the device do (board 2768).
    if writable > MAX_WRITABLE {
        writer.obj(
            room,
            &Response { status: Errno::EINVAL.raw().cast_unsigned().to_le(), reserved: 0 },
        );
        return Handled { len, status: Err(Errno::EINVAL) };
    }
    let served = serve(cell, dialect, bounds, &mut reader, &writer, room).await;
    let (written, status) = match served {
        Ok(written) => (written, Ok(())),
        Err(errno) => (0, Err(errno)),
    };
    // What the request didn't return reads as zeros, as drivers read the buffers whole.
    writer.zero(written, room - written);
    let raw = status.err().map_or(0, |errno| errno.raw().cast_unsigned());
    writer.obj(room, &Response { status: raw.to_le(), reserved: 0 });
    Handled { len, status }
}

/// Serves the request `reader` holds, within `bounds`, writing what it returns with `writer`, which
/// has `room` bytes for it, and returns how many it wrote.
async fn serve<S: Backend, D: Dialect<S>>(
    cell: &S,
    dialect: &D,
    bounds: Bounds,
    reader: &mut Reader<'_>,
    writer: &Writer<'_>,
    room: usize,
) -> Result<usize, Errno> {
    let header: RequestHeader = reader.obj()?;
    let type_ = u32::from_le(header.type_);
    // cladefs: a flag its type doesn't know, refused before anything of it is served (doc-4,
    // board 2774); and a type no request is of.
    let known = dialect.known_flags(type_).ok_or(Errno::EINVAL)?;
    if u32::from_le(header.flags) & !known != 0 {
        return Err(Errno::EINVAL);
    }
    let flags = dialect.served_flags(type_, u32::from_le(header.flags));
    match type_ {
        VIRTIO_EXTENT_T_READ => {
            let args: IoArgs = reader.obj()?;
            let len = u32::from_le(args.len);
            if len > MAX_TRANSFER || room < len as usize {
                return Err(Errno::EINVAL);
            }
            let offset = u64::from_le(args.offset);
            bounds.io(offset, len.into())?;
            let extent = args.id.decode()?;
            let mut slices = writer.slices(0, len as usize).ok_or(Errno::EINVAL)?;
            cell.read(extent, offset, &mut slices).await?;
            Ok(len as usize)
        }
        VIRTIO_EXTENT_T_WRITE => {
            let args: IoArgs = reader.obj()?;
            let len = u32::from_le(args.len);
            let offset = u64::from_le(args.offset);
            if len > MAX_TRANSFER {
                return Err(Errno::EINVAL);
            }
            bounds.io(offset, len.into())?;
            let extent = args.id.decode()?;
            let data = reader.slices(len as usize)?;
            // A barrier before orders, without waiting for the disk or the bucket (doc-4); one
            // after, with WRITE_COMMIT, waits as a COMMIT does. One round trip to the daemon either
            // way.
            let (mut barriers, has_root) =
                crate::protocol::decode_write_barriers(flags, D::WRITE_LOCAL)?;
            // cladefs: the root that commit sets, after the data (WRITE_ROOT).
            let root;
            if has_root {
                let len = u32::from_le(args.root_len);
                crate::protocol::check_root_len(len as usize)?;
                root = reader.bytes(len as usize)?;
                barriers.root = Some(&root);
            }
            cell.write(extent, offset, &data, barriers).await?;
            Ok(0)
        }
        VIRTIO_EXTENT_T_FREE => {
            let id: ExtentIdLe = reader.obj()?;
            let mut extents = vec![id.decode()?];
            // cladefs: the rest of a flush's dead extents, each freed as it would be alone, in one
            // request of the daemon's, not one each (TASK-92.13): the first error fails the
            // request, the extents before it freed (`F_FREE_MANY`); refused past `MAX_MANY_DATA`.
            if flags & VIRTIO_EXTENT_FREE_MANY != 0 {
                if reader.available() > MAX_MANY_DATA {
                    return Err(Errno::EINVAL);
                }
                while reader.available() > 0 {
                    let id: ExtentIdLe = reader.obj()?;
                    extents.push(id.decode()?);
                }
            }
            cell.free_many(&extents).await?;
            Ok(0)
        }
        VIRTIO_EXTENT_T_MAP => {
            let args: MapArgs = reader.obj()?;
            let warm = D::MAP_WARM.is_some_and(|warm| flags & warm != 0);
            // cladefs: every map is read-only; a window that copies on write maps zeros too
            // (backlog doc-13). What each map may say of itself: the dialect's flags but those of
            // the request as a whole.
            let request_flags = VIRTIO_EXTENT_MAP_MANY | D::MAP_WARM.unwrap_or(0);
            let map_flags = known
                & !request_flags
                & if cell.copies_on_write() { u32::MAX } else { !VIRTIO_EXTENT_MAP_ZEROS };
            if flags & !(map_flags | request_flags) != 0 {
                return Err(Errno::EINVAL);
            }
            let mut mappings = Vec::new();
            let mut prefault = Vec::new();
            let mut push = |args: MapArgs, flags: u32| -> Result<(), Errno> {
                let (window_offset, len) =
                    (u64::from_le(args.window_offset), u64::from_le(args.len));
                bounds.window(window_offset, len)?;
                let contents = if flags & VIRTIO_EXTENT_MAP_ZEROS != 0 {
                    MappingContents::Zeros
                } else {
                    let offset = u64::from_le(args.offset);
                    bounds.mapped(offset, len)?;
                    let extent = args.id.decode()?;
                    MappingContents::Extent { extent, offset }
                };
                mappings.push(WindowMapping { window_offset, len, contents });
                if flags & VIRTIO_EXTENT_MAP_PREFAULT != 0 {
                    prefault.push((window_offset, len));
                }
                Ok(())
            };
            // cladefs: each within the window, and an extent's within a u64 (TASK-470), or none is
            // sent.
            push(args, flags & map_flags)?;
            // cladefs: the rest of a chunk's pieces (`F_MAP_MANY`), each with its own flags;
            // refused past `MAX_MANY_DATA`.
            if flags & VIRTIO_EXTENT_MAP_MANY != 0 {
                if reader.available() > MAX_MANY_DATA {
                    return Err(Errno::EINVAL);
                }
                while reader.available() > 0 {
                    let entry: MapEntry = reader.obj()?;
                    let flags = u32::from_le(entry.flags);
                    if flags & !map_flags != 0 {
                        return Err(Errno::EINVAL);
                    }
                    push(entry.map, flags)?;
                }
            }
            // cladefs board 6917: one change of the window, in order: the first error fails the
            // request, those before it mapped, with no trailing concurrent RPCs (TASK-563).
            cell.map_many(&mappings, warm).await?;
            // cladefs: what the guest reads next (`F_MAP_PREFAULT`) is then mapped for it
            // (TASK-112.1).
            for (window_offset, len) in prefault {
                dialect.prefault(cell, window_offset, len);
            }
            Ok(0)
        }
        VIRTIO_EXTENT_T_UNMAP => {
            let args: UnmapArgs = reader.obj()?;
            let (window_offset, len) = (u64::from_le(args.window_offset), u64::from_le(args.len));
            bounds.window(window_offset, len)?;
            cell.unmap(window_offset, len).await?;
            Ok(0)
        }
        VIRTIO_EXTENT_T_COMMIT => {
            let args: CommitArgs = reader.obj()?;
            let (commit, has_root) = crate::protocol::decode_commit_flags(flags, D::COMMIT_LOCAL)?;
            let mut root = None;
            if has_root {
                let len = u32::from_le(args.root_len);
                crate::protocol::check_root_len(len as usize)?;
                root = Some(reader.bytes(len as usize)?);
            }
            cell.commit(root.as_deref(), commit).await?;
            Ok(0)
        }
        VIRTIO_EXTENT_T_ROOT => {
            let returned = MAX_ROOT as usize + size_of::<RootLen>();
            let root = cell.root().await?;
            if room < returned || root.len() > MAX_ROOT as usize {
                return Err(Errno::EINVAL);
            }
            writer.write(0, &root);
            writer.zero(root.len(), MAX_ROOT as usize - root.len());
            let len = u32::try_from(root.len()).expect("the root is short");
            writer.obj(MAX_ROOT as usize, &RootLen { len: len.to_le(), reserved: 0 });
            Ok(returned)
        }
        VIRTIO_EXTENT_T_INFO => {
            // cladefs: the whole of it, the cell too, which a driver without F_FREEZE had no room
            // for, served before: a driver of this protocol has (TASK-141.3).
            if room < size_of::<Info>() {
                return Err(Errno::EINVAL);
            }
            let info = cell.info().await?;
            let info = Info {
                capacity: info.capacity.to_le(),
                used: info.used.to_le(),
                available: info.available.to_le(),
                next_sequence: info.next_sequence.to_le(),
                sequence_floor: info.sequence_floor.to_le(),
                cell: cell.cell().to_le(),
            };
            writer.obj(0, &info);
            Ok(size_of::<Info>())
        }
        _ => dialect.serve_own(cell, type_, flags, bounds, reader, writer, room).await,
    }
}
