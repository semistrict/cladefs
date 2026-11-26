// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The wire protocol between libmachine (the host) and its guest agent, over
//! a vsock stream: one definition, which both sides build from.
//!
//! # Version 3
//!
//! Each connection carries one exchange. Both sides start by sending an
//! 8-byte preamble, `lmag` and the protocol version as a little-endian
//! `u32`, without waiting for the other's. The agent follows its own with a
//! `Hello` frame naming its instance (a random number it picks when it
//! starts, so that the host can tell an agent that restarted, and forgot
//! what it ran, from the one it talked to before) and the generation it
//! last reseeded the guest's RNG for (0 until it has). The host sends its
//! request only once it has read the agent's preamble and `Hello`, so a
//! connection reset before that is known not to have carried the request. A
//! side that reads another version refuses: the agent with an
//! [`Frame::Error`] (`EPROTO`) and a close, the host with an error naming
//! both versions. Versions are never converted: a mismatch is an error.
//!
//! Then frames: a little-endian `u32` length (of what follows), a `u8`
//! type, a payload of at most [`MAX_FRAME`] bytes. Byte strings are a `u32`
//! length and the bytes; lists a `u32` count and the items; numbers
//! little-endian.
//!
//! The exchanges (host frames, then the agent's):
//!
//! - **Reseed:** `Reseed` with a generation and host entropy; `Done` once
//!   the agent has mixed the entropy into the kernel's RNG and had it
//!   reseed (`RNDADDENTROPY`, `RNDRESEEDCRNG`), or `Error`. A machine
//!   restored from a snapshot has a new generation, which its host compares
//!   with the `Hello`'s: until the agent has reseeded for it, the host makes
//!   this exchange before any other, so that nothing runs in a clone with
//!   the random state of the machine it was cloned from.
//! - **Ping:** `Ping`; `Pong`.
//! - **Exec:** `Exec` with a request ID the host chose; `Started` or
//!   `Error`. Then, as they come, the host's `Stdin` (an empty one closes
//!   the process's stdin) and `Signal` (to the process group), and the
//!   agent's `Stdout` and `Stderr`, until the agent's `Exit` once the
//!   process has exited and its output pipes have closed. The host ends
//!   the exchange with `Release`: the agent kills the process group if the
//!   command still runs, and forgets the command.
//! - **Attach:** after a reset, the host takes up a command it started:
//!   `Attach` with its ID and how much of each output stream it has;
//!   `Attached` with how much of the host's stdin and how many signals the
//!   agent has taken, or `Error` (`ESRCH`: no such command; `ENOBUFS`: the
//!   output from there is no longer kept). Then as for `Exec`, the output
//!   from the host's offsets on, the host's stdin from the agent's.
//! - **Read a file:** `ReadFile`; `Data` chunks, then an empty `Data`, or
//!   `Error` at any point.
//! - **Write a file:** `WriteFile`, `Data` chunks, an empty `Data`; `Done`
//!   or `Error` (which may come early). The agent writes a temporary file
//!   beside the target and renames it over the target at the end, so a
//!   write the host makes again after a reset leaves the same file.
//!
//! # Commands outlive their connections
//!
//! The host follows cladefs's reconnect rules (its integrator guide,
//! "Guests' vsock connections across a restore"): a new connection for each
//! exchange; a reset means connect again; only what is safe to see twice
//! is sent again. Pings and files are; a command is made so by its ID:
//!
//! - An `Exec` whose ID the agent knows attaches to that command from its
//!   start instead of running it again, so the host resends an `Exec` after
//!   any reset.
//! - A command whose connection ends without `Release` keeps running
//!   ([`ORPHAN_GRACE`]), its output kept for the host to attach again: each
//!   stream keeps the last [`OUTPUT_KEPT`] bytes it sent (more than can be
//!   in flight on libmachine's vsock) and reads no more from the process
//!   than [`OUTPUT_UNSENT`] ahead of what it sent. Past the grace it is
//!   killed; a finished command's result is kept as long
//!   ([`RESULT_KEPT`]).
//! - The host keeps what it wrote to stdin that may not have arrived and
//!   resends it from the agent's count, and resends signals the agent has
//!   not counted, so each byte and signal arrives once.
//! - An agent that restarted has another instance: the host does not
//!   resend to it a command it may have run.

use std::io::{self, Read, Write};

/// The preamble's magic.
pub const MAGIC: [u8; 4] = *b"lmag";
/// The protocol version this crate speaks.
pub const VERSION: u32 = 3;
/// The vsock port the agent listens on by default: below 1024, so that
/// only root in the guest can listen there.
pub const DEFAULT_PORT: u32 = 100;
/// The most payload one frame may have.
pub const MAX_FRAME: usize = 1 << 20;
/// How much data one `Stdin`, `Stdout`, `Stderr` or `Data` frame carries at
/// most, by convention (any size up to [`MAX_FRAME`] is accepted).
pub const CHUNK: usize = 64 << 10;
/// Linux's EPROTO, for refusing a peer that breaks the protocol.
pub const EPROTO: i32 = 71;
/// Linux's ESRCH: an `Attach` names a command the agent does not have.
pub const ESRCH: i32 = 3;
/// Linux's ENOBUFS: an `Attach` asks for output the agent no longer keeps.
pub const ENOBUFS: i32 = 105;
/// Linux's EAGAIN: the agent has as many commands as it keeps.
pub const EAGAIN: i32 = 11;
/// How much of each output stream the agent keeps after sending it, for a
/// host that attaches again after a reset: more than libmachine's vsock can
/// have in flight (the host's 256 KiB receive credit and a frame).
pub const OUTPUT_KEPT: usize = 512 << 10;
/// How far ahead of what it sent the agent reads a command's output: past
/// it, the command waits to write (while no host is attached, or the host
/// reads slowly), so nothing is lost.
pub const OUTPUT_UNSENT: usize = 256 << 10;
/// How long, in seconds, a command whose connection ended without
/// `Release` keeps running for the host to attach again; then it is killed.
pub const ORPHAN_GRACE: u64 = 60;
/// How long, in seconds, the agent keeps a finished command's result for a
/// host whose connection ended before `Release`.
pub const RESULT_KEPT: u64 = 60;

/// This side's preamble.
pub fn preamble() -> [u8; 8] {
    let mut p = [0; 8];
    p[..4].copy_from_slice(&MAGIC);
    p[4..].copy_from_slice(&VERSION.to_le_bytes());
    p
}

/// Why a peer's preamble was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreambleError {
    /// It does not start with the magic: not this protocol at all.
    NotTheProtocol,
    /// Another version of it.
    Version(u32),
}

impl std::fmt::Display for PreambleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotTheProtocol => {
                write!(f, "the peer does not speak libmachine's agent protocol")
            }
            Self::Version(v) => {
                write!(f, "the peer speaks agent protocol version {v}, this side version {VERSION}")
            }
        }
    }
}

impl std::error::Error for PreambleError {}

/// Checks the peer's preamble.
pub fn check_preamble(p: &[u8; 8]) -> Result<(), PreambleError> {
    if p[..4] != MAGIC {
        return Err(PreambleError::NotTheProtocol);
    }
    match u32::from_le_bytes(p[4..].try_into().unwrap()) {
        VERSION => Ok(()),
        other => Err(PreambleError::Version(other)),
    }
}

/// What to run.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExecRequest {
    /// The program: a path, or a name looked up in `env`'s `PATH`.
    pub program: Vec<u8>,
    /// Its arguments after argv[0] (which is `program`).
    pub args: Vec<Vec<u8>>,
    /// Its whole environment.
    pub env: Vec<(Vec<u8>, Vec<u8>)>,
    /// Its working directory; the agent's if None.
    pub cwd: Option<Vec<u8>>,
    /// Whether its stdin is a pipe the host feeds (else /dev/null).
    pub stdin: bool,
}

/// How a process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// It exited with this status.
    Code(i32),
    /// A signal killed it.
    Signal(i32),
}

/// A frame. The first group goes from the host to the agent, `Data` both
/// ways, the rest from the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    Ping,
    /// Runs a command, or attaches to the one with this ID from its start.
    Exec {
        id: u64,
        request: ExecRequest,
    },
    /// Takes up command `id` again, having its output up to these offsets.
    Attach {
        id: u64,
        stdout: u64,
        stderr: u64,
    },
    /// The host is done with the command: killed if it runs, forgotten.
    Release,
    /// Bytes for the process's stdin; empty closes it.
    Stdin(Vec<u8>),
    /// A signal for the process group.
    Signal(i32),
    ReadFile {
        path: Vec<u8>,
    },
    /// Followed by `Data` frames.
    WriteFile {
        path: Vec<u8>,
        /// Permission bits (`0o7777` at most).
        mode: u32,
    },
    /// A file's bytes; empty ends them.
    Data(Vec<u8>),
    /// Mixes `entropy` into the guest kernel's RNG and has it reseed, for
    /// restore `generation`.
    Reseed {
        generation: u64,
        entropy: Vec<u8>,
    },
    /// The agent's first frame on every connection.
    Hello {
        instance: u64,
        /// The generation the agent last reseeded for (0: none).
        generation: u64,
    },
    Pong,
    Started {
        pid: u32,
    },
    /// Answers `Attach`: how many bytes of stdin and how many signals the
    /// agent has taken for the command.
    Attached {
        stdin: u64,
        signals: u64,
    },
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Exit(Exit),
    /// A file was written.
    Done,
    /// The ask failed in the guest: a Linux errno, and what failed.
    Error {
        errno: i32,
        message: String,
    },
}

mod kind {
    pub const PING: u8 = 0x01;
    pub const EXEC: u8 = 0x02;
    pub const STDIN: u8 = 0x03;
    pub const SIGNAL: u8 = 0x04;
    pub const READ_FILE: u8 = 0x05;
    pub const WRITE_FILE: u8 = 0x06;
    pub const DATA: u8 = 0x07;
    pub const ATTACH: u8 = 0x08;
    pub const RELEASE: u8 = 0x09;
    pub const RESEED: u8 = 0x0a;
    pub const PONG: u8 = 0x81;
    pub const STARTED: u8 = 0x82;
    pub const STDOUT: u8 = 0x83;
    pub const STDERR: u8 = 0x84;
    pub const EXIT: u8 = 0x85;
    pub const DONE: u8 = 0x86;
    pub const ERROR: u8 = 0x87;
    pub const HELLO: u8 = 0x88;
    pub const ATTACHED: u8 = 0x89;
}

struct Encoder(Vec<u8>);

impl Encoder {
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn i32(&mut self, v: i32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn bytes(&mut self, b: &[u8]) {
        self.u32(b.len() as u32);
        self.0.extend_from_slice(b);
    }
}

struct Decoder<'a>(&'a [u8]);

fn invalid(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("agent protocol: {what}"))
}

impl<'a> Decoder<'a> {
    fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
        if self.0.len() < n {
            return Err(invalid("frame too short"));
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(head)
    }
    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn i32(&mut self) -> io::Result<i32> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn u8(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn bytes(&mut self) -> io::Result<Vec<u8>> {
        let n = self.u32()? as usize;
        Ok(self.take(n)?.to_vec())
    }
    /// A list's count, checked against what is left (each item takes at
    /// least `min_item` bytes), so a count cannot make the reader allocate
    /// more than the frame holds.
    fn count(&mut self, min_item: usize) -> io::Result<usize> {
        let n = self.u32()? as usize;
        if n.saturating_mul(min_item) > self.0.len() {
            return Err(invalid("list longer than its frame"));
        }
        Ok(n)
    }
    fn rest(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.0).to_vec()
    }
    fn end(&self) -> io::Result<()> {
        if self.0.is_empty() { Ok(()) } else { Err(invalid("trailing bytes in a frame")) }
    }
}

impl Frame {
    /// The frame as it goes on the wire, length first.
    pub fn encode(&self) -> Vec<u8> {
        let mut e = Encoder(vec![0; 5]);
        let kind = match self {
            Frame::Ping => kind::PING,
            Frame::Exec { id, request: r } => {
                e.u64(*id);
                e.u32(u32::from(r.stdin));
                e.bytes(&r.program);
                e.u32(r.args.len() as u32);
                for a in &r.args {
                    e.bytes(a);
                }
                e.u32(r.env.len() as u32);
                for (k, v) in &r.env {
                    e.bytes(k);
                    e.bytes(v);
                }
                match &r.cwd {
                    None => e.0.push(0),
                    Some(cwd) => {
                        e.0.push(1);
                        e.bytes(cwd);
                    }
                }
                kind::EXEC
            }
            Frame::Stdin(d) => {
                e.0.extend_from_slice(d);
                kind::STDIN
            }
            Frame::Signal(s) => {
                e.i32(*s);
                kind::SIGNAL
            }
            Frame::ReadFile { path } => {
                e.bytes(path);
                kind::READ_FILE
            }
            Frame::WriteFile { path, mode } => {
                e.bytes(path);
                e.u32(*mode);
                kind::WRITE_FILE
            }
            Frame::Data(d) => {
                e.0.extend_from_slice(d);
                kind::DATA
            }
            Frame::Attach { id, stdout, stderr } => {
                e.u64(*id);
                e.u64(*stdout);
                e.u64(*stderr);
                kind::ATTACH
            }
            Frame::Release => kind::RELEASE,
            Frame::Reseed { generation, entropy } => {
                e.u64(*generation);
                e.bytes(entropy);
                kind::RESEED
            }
            Frame::Hello { instance, generation } => {
                e.u64(*instance);
                e.u64(*generation);
                kind::HELLO
            }
            Frame::Pong => kind::PONG,
            Frame::Started { pid } => {
                e.u32(*pid);
                kind::STARTED
            }
            Frame::Attached { stdin, signals } => {
                e.u64(*stdin);
                e.u64(*signals);
                kind::ATTACHED
            }
            Frame::Stdout(d) => {
                e.0.extend_from_slice(d);
                kind::STDOUT
            }
            Frame::Stderr(d) => {
                e.0.extend_from_slice(d);
                kind::STDERR
            }
            Frame::Exit(exit) => {
                let (tag, value) = match exit {
                    Exit::Code(c) => (0, *c),
                    Exit::Signal(s) => (1, *s),
                };
                e.0.push(tag);
                e.i32(value);
                kind::EXIT
            }
            Frame::Done => kind::DONE,
            Frame::Error { errno, message } => {
                e.i32(*errno);
                e.bytes(message.as_bytes());
                kind::ERROR
            }
        };
        let len = (e.0.len() - 4) as u32;
        e.0[..4].copy_from_slice(&len.to_le_bytes());
        e.0[4] = kind;
        e.0
    }

    /// A frame from its type and payload.
    pub fn decode(kind: u8, payload: &[u8]) -> io::Result<Frame> {
        let mut d = Decoder(payload);
        let frame = match kind {
            kind::PING => Frame::Ping,
            kind::EXEC => {
                let id = d.u64()?;
                let stdin = match d.u32()? {
                    0 => false,
                    1 => true,
                    _ => return Err(invalid("bad exec flags")),
                };
                let program = d.bytes()?;
                let args = (0..d.count(4)?).map(|_| d.bytes()).collect::<io::Result<_>>()?;
                let env = (0..d.count(8)?)
                    .map(|_| Ok((d.bytes()?, d.bytes()?)))
                    .collect::<io::Result<_>>()?;
                let cwd = match d.u8()? {
                    0 => None,
                    1 => Some(d.bytes()?),
                    _ => return Err(invalid("bad exec cwd")),
                };
                Frame::Exec { id, request: ExecRequest { program, args, env, cwd, stdin } }
            }
            kind::STDIN => Frame::Stdin(d.rest()),
            kind::SIGNAL => Frame::Signal(d.i32()?),
            kind::READ_FILE => Frame::ReadFile { path: d.bytes()? },
            kind::WRITE_FILE => Frame::WriteFile { path: d.bytes()?, mode: d.u32()? },
            kind::DATA => Frame::Data(d.rest()),
            kind::ATTACH => Frame::Attach { id: d.u64()?, stdout: d.u64()?, stderr: d.u64()? },
            kind::RELEASE => Frame::Release,
            kind::RESEED => Frame::Reseed { generation: d.u64()?, entropy: d.bytes()? },
            kind::HELLO => Frame::Hello { instance: d.u64()?, generation: d.u64()? },
            kind::PONG => Frame::Pong,
            kind::STARTED => Frame::Started { pid: d.u32()? },
            kind::ATTACHED => Frame::Attached { stdin: d.u64()?, signals: d.u64()? },
            kind::STDOUT => Frame::Stdout(d.rest()),
            kind::STDERR => Frame::Stderr(d.rest()),
            kind::EXIT => Frame::Exit(match (d.u8()?, d.i32()?) {
                (0, c) => Exit::Code(c),
                (1, s) => Exit::Signal(s),
                _ => return Err(invalid("bad exit")),
            }),
            kind::DONE => Frame::Done,
            kind::ERROR => Frame::Error {
                errno: d.i32()?,
                message: String::from_utf8_lossy(&d.bytes()?).into_owned(),
            },
            other => return Err(invalid(&format!("unknown frame type {other:#x}"))),
        };
        d.end()?;
        Ok(frame)
    }
}

impl Frame {
    /// The frame's type, for messages.
    pub fn name(&self) -> &'static str {
        match self {
            Frame::Ping => "Ping",
            Frame::Exec { .. } => "Exec",
            Frame::Attach { .. } => "Attach",
            Frame::Release => "Release",
            Frame::Stdin(_) => "Stdin",
            Frame::Signal(_) => "Signal",
            Frame::ReadFile { .. } => "ReadFile",
            Frame::WriteFile { .. } => "WriteFile",
            Frame::Data(_) => "Data",
            Frame::Reseed { .. } => "Reseed",
            Frame::Hello { .. } => "Hello",
            Frame::Pong => "Pong",
            Frame::Started { .. } => "Started",
            Frame::Attached { .. } => "Attached",
            Frame::Stdout(_) => "Stdout",
            Frame::Stderr(_) => "Stderr",
            Frame::Exit(_) => "Exit",
            Frame::Done => "Done",
            Frame::Error { .. } => "Error",
        }
    }
}

/// Writes `frame` (in one write, so that frames from several threads behind
/// one lock never interleave).
pub fn write_frame(mut w: impl Write, frame: &Frame) -> io::Result<()> {
    w.write_all(&frame.encode())
}

/// Reads a frame; `None` if the stream ends before one starts. A stream
/// that ends inside a frame fails with `UnexpectedEof`.
pub fn read_frame(mut r: impl Read) -> io::Result<Option<Frame>> {
    let mut len = [0; 4];
    let mut got = 0;
    while got < len.len() {
        match r.read(&mut len[got..]) {
            Ok(0) if got == 0 => return Ok(None),
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    let len = u32::from_le_bytes(len) as usize;
    if len == 0 || len - 1 > MAX_FRAME {
        return Err(invalid(&format!("frame of {len} bytes")));
    }
    let mut body = vec![0; len];
    r.read_exact(&mut body)?;
    Frame::decode(body[0], &body[1..]).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_frames() -> Vec<Frame> {
        vec![
            Frame::Ping,
            Frame::Exec {
                id: 0x0123_4567_89ab_cdef,
                request: ExecRequest {
                    program: b"/bin/sh".to_vec(),
                    args: vec![b"-c".to_vec(), b"echo hi".to_vec(), Vec::new()],
                    env: vec![(b"PATH".to_vec(), b"/bin".to_vec())],
                    cwd: Some(b"/tmp".to_vec()),
                    stdin: true,
                },
            },
            Frame::Exec { id: 0, request: ExecRequest::default() },
            Frame::Attach { id: u64::MAX, stdout: 1 << 40, stderr: 3 },
            Frame::Release,
            Frame::Hello { instance: 99, generation: 7 },
            Frame::Reseed { generation: u64::MAX, entropy: vec![0xa5; 64] },
            Frame::Attached { stdin: 5, signals: 2 },
            Frame::Stdin(b"input".to_vec()),
            Frame::Stdin(Vec::new()),
            Frame::Signal(15),
            Frame::ReadFile { path: b"/etc/hostname".to_vec() },
            Frame::WriteFile { path: b"/tmp/x".to_vec(), mode: 0o755 },
            Frame::Data(vec![0; CHUNK]),
            Frame::Data(Vec::new()),
            Frame::Pong,
            Frame::Started { pid: 42 },
            Frame::Stdout(b"out".to_vec()),
            Frame::Stderr(b"err".to_vec()),
            Frame::Exit(Exit::Code(-1)),
            Frame::Exit(Exit::Signal(9)),
            Frame::Done,
            Frame::Error { errno: 2, message: "no such file".into() },
        ]
    }

    #[test]
    fn every_frame_round_trips() {
        let mut wire = Vec::new();
        for f in all_frames() {
            write_frame(&mut wire, &f).unwrap();
        }
        let mut r = &wire[..];
        for f in all_frames() {
            assert_eq!(read_frame(&mut r).unwrap(), Some(f));
        }
        assert_eq!(read_frame(&mut r).unwrap(), None, "clean end");
    }

    #[test]
    fn the_wire_layout_is_fixed() {
        assert_eq!(preamble(), *b"lmag\x03\0\0\0");
        assert_eq!(
            Frame::Signal(9).encode(),
            [5, 0, 0, 0, 0x04, 9, 0, 0, 0],
            "length, type, payload"
        );
        assert_eq!(
            Frame::ReadFile { path: b"/a".to_vec() }.encode(),
            [7, 0, 0, 0, 0x05, 2, 0, 0, 0, b'/', b'a']
        );
        assert_eq!(
            Frame::Hello { instance: 0x0102, generation: 3 }.encode(),
            [17, 0, 0, 0, 0x88, 2, 1, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(
            Frame::Reseed { generation: 1, entropy: vec![0xee] }.encode(),
            [14, 0, 0, 0, 0x0a, 1, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0xee]
        );
        assert_eq!(Frame::Release.encode(), [1, 0, 0, 0, 0x09]);
        let attach = Frame::Attach { id: 1, stdout: 2, stderr: 3 }.encode();
        assert_eq!(&attach[..5], [25, 0, 0, 0, 0x08]);
        assert_eq!(attach[5..].chunks(8).map(|c| c[0]).collect::<Vec<_>>(), [1, 2, 3]);
        let exec = Frame::Exec { id: 7, request: ExecRequest::default() }.encode();
        assert_eq!(exec[4], 0x02);
        assert_eq!(exec[5..13], 7u64.to_le_bytes(), "the ID comes first");
        for f in all_frames() {
            assert!(!f.name().is_empty());
        }
    }

    #[test]
    fn preambles_of_other_versions_are_refused() {
        assert_eq!(check_preamble(&preamble()), Ok(()));
        // Versions 1 (no request IDs) and 2 (no reseed), and any later one,
        // are refused, never converted.
        for other in [1u32, 2, VERSION + 1, u32::MAX] {
            let mut p = preamble();
            p[4..].copy_from_slice(&other.to_le_bytes());
            assert_eq!(check_preamble(&p), Err(PreambleError::Version(other)));
        }
        let message = PreambleError::Version(1).to_string();
        assert!(
            message.contains("version 1") && message.contains(&format!("version {VERSION}")),
            "{message}"
        );
        assert_eq!(check_preamble(b"GET / HT"), Err(PreambleError::NotTheProtocol));
    }

    #[test]
    fn bad_frames_are_invalid_data() {
        let bad: Vec<Vec<u8>> = vec![
            vec![0, 0, 0, 0],                                // no type
            vec![1, 0, 0, 0, 0x7f],                          // unknown type
            vec![2, 0, 0, 0, 0x01, 0],                       // trailing byte
            vec![3, 0, 0, 0, 0x04, 1, 2],                    // short signal
            ((MAX_FRAME + 2) as u32).to_le_bytes().to_vec(), // too long
            // An exec whose argument count is far beyond its frame.
            {
                let mut f = vec![0, 0, 0, 0, 0x02];
                f.extend_from_slice(&0u64.to_le_bytes());
                f.extend_from_slice(&0u32.to_le_bytes());
                f.extend_from_slice(&0u32.to_le_bytes());
                f.extend_from_slice(&u32::MAX.to_le_bytes());
                let len = (f.len() - 4) as u32;
                f[..4].copy_from_slice(&len.to_le_bytes());
                f
            },
        ];
        for b in bad {
            let err = read_frame(&b[..]).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{b:?}");
        }
        // Cut short inside a frame.
        let whole = Frame::Stdout(b"abc".to_vec()).encode();
        for cut in 1..whole.len() {
            let err = read_frame(&whole[..cut]).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof, "cut at {cut}");
        }
    }

    /// Any bytes decode to a frame or an error, never a panic: the decoder
    /// reads what an untrusted guest sends.
    #[test]
    fn arbitrary_bytes_never_panic() {
        let mut x = 0x2545_f491_4f6c_dd1du64;
        for f in all_frames() {
            let wire = f.encode();
            for _ in 0..200 {
                let mut b = wire.clone();
                for _ in 0..3 {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    let i = (x as usize) % b.len();
                    b[i] = (x >> 32) as u8;
                }
                let _ = read_frame(&b[..]);
            }
        }
    }
}
