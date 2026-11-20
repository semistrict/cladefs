// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: a simulated guest, under madsim: a VM process's side of a connection to its host's
//! daemon, in memory (`server::connect_in_memory`), as the simulation's guests have no VM socket
//! (doc-24's boundary, TASK-685.6). Its requests are the wire's, encoded by the codec as a VM
//! process's are, served by the daemon's own handling of them: OPEN, READ, WRITE, COMMIT at each
//! level, FREE, versions. No window: nothing is mapped, no stage operation made. The daemon's asks
//! of it, its stats and a shrink, it answers as a VM process without a guest of its own would.

use super::codec::{Data, Forked, Opened, Opening, ReadInto, Request, Told};
use super::wire::{AsyncReceiver, AsyncSender, Header, Message, REPLY};
use crate::{Commit, Errno, ExtentId, WriteBarriers};
use bucket_cell::cell::Kind;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// The replies a guest waits for, by the IDs of their requests; none once its connection went.
type Waiting = Arc<Mutex<Option<HashMap<u64, futures::channel::oneshot::Sender<Message>>>>>;

/// A simulated guest's connection to its host's daemon (see the module's docs).
pub struct Guest {
    sender: Arc<AsyncSender>,
    next: AtomicU64,
    waiting: Waiting,
}

impl Guest {
    /// A guest of the daemon of this simulated host, of its default tenant, its connection served
    /// as a VM process's is.
    pub fn connect() -> Result<Self, Errno> {
        let tenant = super::server::default_tenant()?;
        let stream = super::server::connect_in_memory(tenant);
        let failed = |err: std::io::Error| {
            log::error!("a simulated guest's connection: {err}");
            Errno::EIO
        };
        let sender =
            Arc::new(AsyncSender::new(stream.try_clone().map_err(failed)?).map_err(failed)?);
        let receiver = AsyncReceiver::new(stream).map_err(failed)?;
        let waiting: Waiting = Arc::new(Mutex::new(Some(HashMap::new())));
        drop(tokio::spawn(receive(receiver, sender.clone(), waiting.clone())));
        Ok(Self { sender, next: AtomicU64::new(1), waiting })
    }

    /// Sends `request` and returns its reply: `Err` with the errno it failed with, or `EIO` once
    /// the connection went.
    async fn call(&self, request: &Request<'_>) -> Result<Message, Errno> {
        let encoded = request.encode();
        let mut header = encoded.header;
        header.id = self.next.fetch_add(1, Ordering::Relaxed);
        let payload: Vec<u8> =
            encoded.payload.iter().flat_map(|part| part.iter().copied()).collect();
        header.len = payload.len() as u64;
        let (reply, replied) = futures::channel::oneshot::channel();
        match self.waiting.lock().unwrap().as_mut() {
            Some(waiting) => drop(waiting.insert(header.id, reply)),
            None => return Err(Errno::EIO),
        }
        let fds: Vec<_> = encoded.fd.into_iter().collect();
        if self.sender.send(header, payload, &fds).is_err() {
            return Err(Errno::EIO);
        }
        let message = replied.await.map_err(|_| Errno::EIO)?;
        match message.header.status {
            0 => Ok(message),
            status => Err(Errno::from_raw(-status)),
        }
    }

    /// Opens the store at `path` to write, made of `capacity` bytes if it isn't there: its cell's
    /// ID.
    pub async fn open(&self, path: &Path, capacity: u64) -> Result<u64, Errno> {
        let opening = Opening::Create(capacity, Kind::Fxfs);
        let reply = self.call(&Request::Open { path: path.to_owned(), opening }).await?;
        Ok(Opened::from_args(reply.header.args)?.cell)
    }

    /// Writes `data` at `offset` in `extent`.
    pub async fn write(&self, extent: ExtentId, offset: u64, data: &[u8]) -> Result<(), Errno> {
        let barriers = WriteBarriers::default();
        let data = Data::Inline(data);
        self.call(&Request::Write { extent, offset, data, barriers }).await.map(drop)
    }

    /// Commits what it wrote, the commit complete as `commit` says.
    pub async fn commit(&self, commit: Commit) -> Result<(), Errno> {
        self.call(&Request::Commit { commit, root: None }).await.map(drop)
    }

    /// `len` bytes at `offset` in `extent`.
    pub async fn read(&self, extent: ExtentId, offset: u64, len: u64) -> Result<Vec<u8>, Errno> {
        let read = Request::Read { extent, offset, len, into: ReadInto::Reply };
        Ok(self.call(&read).await?.payload)
    }

    /// The sequence its store gives the next extent minted: as a guest that took the cell over
    /// mints from.
    pub async fn next_sequence(&self) -> Result<u64, Errno> {
        let reply = self.call(&Request::Info).await?;
        Ok(super::codec::Info::from_args(reply.header.args).0.next_sequence)
    }

    /// Frees `extent`.
    pub async fn free(&self, extent: ExtentId) -> Result<(), Errno> {
        self.call(&Request::Free(extent)).await.map(drop)
    }

    /// Makes a version of its store's cell, as a VM snapshot does: its number. cladefs: the reply's
    /// version, as `CellClient::make_version` reads it, not its first argument, the cell's
    /// (TASK-698.5 found it so).
    pub async fn make_version(&self) -> Result<u64, Errno> {
        Ok(Forked::from_args(self.call(&Request::Version).await?.header.args).version)
    }

    /// Closes its connection, as a VM process that exits does: the daemon lets its store go once
    /// what it committed is durable.
    pub fn close(self) {
        self.sender.shutdown();
    }
}

/// Receives the daemon's messages to the guest at the other end of `receiver`, until it hangs up:
/// each reply to the request waiting for it; its asks answered through `sender`, its stats with
/// none, a shrink refused.
async fn receive(mut receiver: AsyncReceiver, sender: Arc<AsyncSender>, waiting: Waiting) {
    loop {
        let message = match receiver.receive(|_: &Header| async { Ok(()) }).await {
            Ok(Some((message, ()))) => message,
            Ok(None) => break,
            Err(err) => {
                log::warn!("a simulated guest's connection: {err}");
                break;
            }
        };
        let header = message.header;
        if header.kind == REPLY {
            let replied =
                waiting.lock().unwrap().as_mut().and_then(|waiting| waiting.remove(&header.id));
            if let Some(replied) = replied {
                let _ = replied.send(message);
            }
            continue;
        }
        let answer = match Told::decode(&header) {
            Ok(Told::Stats { ask }) => Request::Stats { ask, guest: None },
            Ok(Told::Capacity { capacity, ask: true }) => {
                Request::Resized { capacity, took: false }
            }
            Ok(Told::Capacity { ask: false, .. } | Told::Committed { .. }) => continue,
            Ok(Told::Stage(stage)) => {
                log::error!(
                    "a simulated guest, with no window, told of a stage operation: {stage:?}"
                );
                break;
            }
            Err(errno) => {
                log::error!(
                    "a simulated guest refuses the daemon's message ({errno:?}): {header:?}"
                );
                break;
            }
        };
        let encoded = answer.encode();
        let payload: Vec<u8> =
            encoded.payload.iter().flat_map(|part| part.iter().copied()).collect();
        let mut header = encoded.header;
        header.len = payload.len() as u64;
        if sender.send(header, payload, &[]).is_err() {
            break;
        }
    }
    // Those waiting, answered with nothing: the connection went.
    drop(waiting.lock().unwrap().take());
}
