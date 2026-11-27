// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! What machined and a machine's helper process say to each other, over the socket that is the
//! helper's standard input: frames of a little-endian u32 length and a protobuf message, at most
//! [`MAX_FRAME`] bytes. It is the build's own (machined runs the helper of its own build, from
//! beside it), unfrozen: a start of another [`PROTOCOL`] is refused.
//!
//! - **machined to the helper:** [`Start`] first, once; then [`Empty`], to stop the machine.
//! - **The helper to machined:** [`event::Kind::Started`] once the machine runs (or
//!   [`Ended`] if it never did); its console's bytes as they come; [`Ended`] last, once it is
//!   freed.

use std::io;

use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

/// The wire's version: a helper of another refuses its start.
pub(crate) const PROTOCOL: u32 = 2;
/// The largest frame either side takes: a console chunk, a spec.
pub(crate) const MAX_FRAME: usize = 4 << 20;

/// How the helper starts its machine.
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct Start {
    #[prost(uint32, tag = "1")]
    pub(crate) protocol: u32,
    /// The machine, as its caller gave it: the helper checks and plans it again, as machined did.
    #[prost(message, optional, tag = "2")]
    pub(crate) spec: Option<crate::proto::MachineSpec>,
    /// machined's images directory, which the spec's paths are in.
    #[prost(string, tag = "3")]
    pub(crate) images: String,
    /// Where the helper serves the machine's vsock socket.
    #[prost(string, tag = "4")]
    pub(crate) vsock_socket: String,
    /// Where machined's cladefs daemon serves VM processes, which the machine's extent store
    /// devices (its root's) connect to.
    #[prost(string, tag = "5")]
    pub(crate) cladefs_socket: String,
}

/// The frame that stops the machine, and the one that says it runs.
#[derive(Clone, Copy, PartialEq, Eq, prost::Message)]
pub(crate) struct Empty {}

/// What the helper says.
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct Event {
    #[prost(oneof = "event::Kind", tags = "1, 2, 3")]
    pub(crate) kind: Option<event::Kind>,
}

pub(crate) mod event {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub(crate) enum Kind {
        /// The machine runs.
        #[prost(message, tag = "1")]
        Started(super::Empty),
        /// Its console's bytes.
        #[prost(bytes = "vec", tag = "2")]
        Console(Vec<u8>),
        /// How it ended: the last event.
        #[prost(message, tag = "3")]
        Ended(super::Ended),
    }
}

/// How a machine ended.
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct Ended {
    #[prost(enumeration = "How", tag = "1")]
    pub(crate) how: i32,
    /// What failed, for FAILED and BOOT_FAILED.
    #[prost(string, tag = "2")]
    pub(crate) detail: String,
    /// BOOT_FAILED's status code (machined's API's).
    #[prost(int32, tag = "3")]
    pub(crate) code: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, prost::Enumeration)]
#[repr(i32)]
pub(crate) enum How {
    Unspecified = 0,
    /// machined stopped it.
    Stopped = 1,
    Poweroff = 2,
    Reset = 3,
    Shutdown = 4,
    /// Running it failed, or its guest made an exit libmachine doesn't handle.
    Failed = 5,
    /// It never ran: making it failed.
    BootFailed = 6,
}

/// Writes `message` as a frame.
pub(crate) async fn write<W: AsyncWrite + Unpin>(
    w: &mut W,
    message: &impl prost::Message,
) -> io::Result<()> {
    let body = message.encode_to_vec();
    if body.len() > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "a helper frame past its bound"));
    }
    let len = u32::try_from(body.len()).map_err(io::Error::other)?;
    let mut frame = Vec::with_capacity(body.len() + 4);
    frame.extend_from_slice(&len.to_le_bytes());
    frame.extend_from_slice(&body);
    w.write_all(&frame).await?;
    w.flush().await
}

/// Reads a frame's message: None at the end of the stream, between frames.
pub(crate) async fn read<R: AsyncRead + Unpin, M: prost::Message + Default>(
    r: &mut R,
) -> io::Result<Option<M>> {
    let mut len = [0; 4];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = usize::try_from(u32::from_le_bytes(len)).map_err(io::Error::other)?;
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("a helper frame of {len} bytes"),
        ));
    }
    let mut body = vec![0; len];
    r.read_exact(&mut body).await?;
    M::decode(body.as_slice()).map(Some).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}
