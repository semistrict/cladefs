// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: a VM process's connection to the daemon in memory, under madsim, which has no Unix
//! sockets nor a reactor for them (doc-24's boundary): a pair of ends ([`Stream::pair`]), the
//! daemon's and a simulated guest's, each sending bytes and descriptors as a Unix stream does to
//! the other, the descriptors with the first byte of what they were sent with, and reading them,
//! as the other end sent them, on the simulation's one thread. The protocol above is the daemon's
//! own (`super`): only the socket's calls are these.

use event_listener::Event;
use std::collections::VecDeque;
use std::io;
use std::os::fd::{BorrowedFd, OwnedFd};
use std::sync::{Arc, Mutex};

/// One way of a connection: what one end sent, the other has not read.
#[derive(Default)]
struct Pipe {
    state: Mutex<Piped>,
    /// Said as more is sent, or it closes.
    more: Event,
}

#[derive(Default)]
struct Piped {
    bytes: VecDeque<u8>,
    /// How many bytes were read of it, all along.
    read: u64,
    /// The descriptors sent, each with where its bytes began, as counted by `read`.
    fds: VecDeque<(u64, Vec<OwnedFd>)>,
    /// Whether it takes no more: an end shut it down, or went.
    closed: bool,
}

impl Pipe {
    fn close(&self) {
        self.state.lock().unwrap().closed = true;
        self.more.notify(usize::MAX);
    }

    fn is_closed(&self) -> bool {
        self.state.lock().unwrap().closed
    }
}

/// What closes an end's pipes as its last handle goes, as a socket's last descriptor's closing
/// does: the other end reads what is left, then the end of it.
struct Alive {
    sending: Arc<Pipe>,
    receiving: Arc<Pipe>,
}

impl Drop for Alive {
    fn drop(&mut self) {
        self.sending.close();
        self.receiving.close();
    }
}

/// One end of a connection in memory: what it sends, the other end reads, and the other way round.
/// Cloned as a socket's descriptor is: one end still, its last clone's going its closing.
#[derive(Clone)]
pub struct Stream {
    sending: Arc<Pipe>,
    receiving: Arc<Pipe>,
    _alive: Arc<Alive>,
}

impl Stream {
    /// A connection's two ends.
    pub fn pair() -> (Self, Self) {
        let (there, back) = (Arc::new(Pipe::default()), Arc::new(Pipe::default()));
        let end = |sending: &Arc<Pipe>, receiving: &Arc<Pipe>| {
            let alive = Alive { sending: sending.clone(), receiving: receiving.clone() };
            Self { sending: sending.clone(), receiving: receiving.clone(), _alive: Arc::new(alive) }
        };
        (end(&there, &back), end(&back, &there))
    }

    /// The same end, as `UnixStream::try_clone` gives a socket's.
    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(self.clone())
    }

    /// As a socket's: nothing here blocks the thread.
    pub fn set_nonblocking(&self, _: bool) -> io::Result<()> {
        Ok(())
    }

    /// Shuts the connection down both ways: the other end reads what is left, then its end.
    pub fn shutdown(&self, _: std::net::Shutdown) -> io::Result<()> {
        self.sending.close();
        self.receiving.close();
        Ok(())
    }

    /// Whether the other end hung up: it shut the connection down, or went.
    pub fn hung_up(&self) -> bool {
        self.receiving.is_closed()
    }

    /// Sends `parts`, in order, and `fds` with their first byte: all of them, at once. Fails as a
    /// socket the other end shut down does, `EPIPE`.
    pub fn send_parts(&self, parts: &[&[u8]], fds: &[BorrowedFd<'_>]) -> io::Result<usize> {
        let fds = fds.iter().map(|fd| fd.try_clone_to_owned()).collect::<io::Result<Vec<_>>>()?;
        let mut piped = self.sending.state.lock().unwrap();
        if piped.closed {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        let at = piped.read + piped.bytes.len() as u64;
        if !fds.is_empty() {
            piped.fds.push_back((at, fds));
        }
        let mut sent = 0;
        for part in parts {
            piped.bytes.extend(part.iter());
            sent += part.len();
        }
        drop(piped);
        self.sending.more.notify(usize::MAX);
        Ok(sent)
    }

    /// Reads what of `bytes` the other end sent, once it sent any: with the descriptors that came
    /// with them, if `with_fds`, or none, those dropped, as a socket's are read without room for
    /// them. Nothing, once it hung up, all it sent read.
    pub async fn read(
        &self,
        bytes: &mut [u8],
        with_fds: bool,
    ) -> io::Result<(usize, Vec<OwnedFd>)> {
        loop {
            let more = self.receiving.more.listen();
            {
                let mut piped = self.receiving.state.lock().unwrap();
                if !piped.bytes.is_empty() {
                    // The bytes up to the next descriptors', which begin a read of their own, as
                    // a socket's message with descriptors does.
                    let next = piped.fds.iter().map(|(at, _)| *at).find(|at| *at > piped.read);
                    let most = next.map_or(usize::MAX, |next| (next - piped.read) as usize);
                    let read = bytes.len().min(piped.bytes.len()).min(most);
                    for (into, byte) in bytes.iter_mut().zip(piped.bytes.drain(..read)) {
                        *into = byte;
                    }
                    let (from, to) = (piped.read, piped.read + read as u64);
                    piped.read = to;
                    let mut fds = Vec::new();
                    while piped.fds.front().is_some_and(|(at, _)| *at >= from && *at < to) {
                        let (_, mut theirs) = piped.fds.pop_front().unwrap();
                        if with_fds {
                            fds.append(&mut theirs);
                        }
                    }
                    return Ok((read, fds));
                }
                if piped.closed {
                    return Ok((0, Vec::new()));
                }
            }
            more.await;
        }
    }
}

/// A Unix stream, which a simulated host has none of: as an accepting socket's, which the
/// simulation's daemons serve none of, their guests connecting in memory.
impl From<std::os::unix::net::UnixStream> for Stream {
    fn from(_: std::os::unix::net::UnixStream) -> Self {
        panic!("a Unix stream, which the simulation has none of: its guests connect in memory")
    }
}
