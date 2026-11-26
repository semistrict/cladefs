// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! What machined and a machine's helper process say to each other, over the socket that is the
//! helper's standard input: frames of a little-endian u32 length and a protobuf message, at most
//! [`MAX_FRAME`] bytes. It is the build's own (machined runs the helper of its own build, from
//! beside it), unfrozen: a start of another [`PROTOCOL`] is refused.
//!
//! - **machined to the helper:** [`Start`] first, once; then [`Request`]s: stop, snapshot,
//!   suspend, resume and balloon, as asked.
//! - **The helper to machined:** [`Started`] once the machine runs (or [`Ended`] if it never
//!   did); its console's bytes as they come; whether its guest is suspended as that changes; an
//!   answer to each snapshot, suspend and resume, in the order they were asked; [`Ended`] last,
//!   once it is freed.

use std::io;

use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

/// The wire's version: a helper of another refuses its start.
pub(crate) const PROTOCOL: u32 = 1;
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
    /// A snapshot's file to restore from (KVM); empty to boot the spec's kernel.
    #[prost(string, tag = "4")]
    pub(crate) snapshot: String,
    /// Where the helper serves the machine's vsock socket.
    #[prost(string, tag = "5")]
    pub(crate) vsock_socket: String,
    /// Where machined's cladefs daemon serves VM processes, which the machine's extent store
    /// devices (its root's) connect to.
    #[prost(string, tag = "6")]
    pub(crate) cladefs_socket: String,
}

/// What machined asks of a running machine.
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct Request {
    #[prost(oneof = "request::Kind", tags = "2, 3, 4, 5, 6")]
    pub(crate) kind: Option<request::Kind>,
}

pub(crate) mod request {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub(crate) enum Kind {
        /// Stops the machine.
        #[prost(message, tag = "2")]
        Stop(super::Empty),
        /// Writes a snapshot of it to this file.
        #[prost(string, tag = "3")]
        Snapshot(String),
        /// Suspends its idle guest into a file in this directory.
        #[prost(string, tag = "4")]
        Suspend(String),
        /// Wakes its suspended guest.
        #[prost(message, tag = "5")]
        Resume(super::Empty),
        /// Sets its balloon's target, if given, and says how it stands.
        #[prost(message, tag = "6")]
        Balloon(super::BalloonTarget),
    }
}

#[derive(Clone, Copy, PartialEq, Eq, prost::Message)]
pub(crate) struct Empty {}

#[derive(Clone, Copy, PartialEq, Eq, prost::Message)]
pub(crate) struct BalloonTarget {
    #[prost(uint64, optional, tag = "1")]
    pub(crate) bytes: Option<u64>,
}

/// What the helper says.
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct Event {
    #[prost(oneof = "event::Kind", tags = "1, 2, 3, 4, 5")]
    pub(crate) kind: Option<event::Kind>,
}

pub(crate) mod event {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub(crate) enum Kind {
        /// The machine runs, of this vsock generation (0 if it booted).
        #[prost(uint64, tag = "1")]
        Started(u64),
        /// Its console's bytes.
        #[prost(bytes = "vec", tag = "2")]
        Console(Vec<u8>),
        /// Whether its guest is suspended now.
        #[prost(bool, tag = "3")]
        Suspended(bool),
        /// The answer to the oldest request not yet answered.
        #[prost(message, tag = "4")]
        Answer(super::Answer),
        /// How it ended: the last event.
        #[prost(message, tag = "5")]
        Ended(super::Ended),
    }
}

/// An answer: done, or a status's code and message (machined's API's, as `errors::status`
/// makes them).
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct Answer {
    #[prost(int32, tag = "1")]
    pub(crate) code: i32,
    #[prost(string, tag = "2")]
    pub(crate) message: String,
    /// A balloon's, as it stands, answering a Balloon request.
    #[prost(message, optional, tag = "3")]
    pub(crate) balloon: Option<crate::proto::BalloonStatus>,
}

impl Answer {
    pub(crate) fn of(result: Result<Option<crate::proto::BalloonStatus>, tonic::Status>) -> Self {
        match result {
            Ok(balloon) => Self { code: 0, message: String::new(), balloon },
            Err(status) => {
                Self { code: status.code().into(), message: status.message().into(), balloon: None }
            }
        }
    }

    pub(crate) fn result(self) -> Result<Option<crate::proto::BalloonStatus>, tonic::Status> {
        match self.code {
            0 => Ok(self.balloon),
            code => Err(tonic::Status::new(code.into(), self.message)),
        }
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
