// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Calls to a running machine's guest through libmachine's agent, which block their thread while
//! the guest takes its time, or forever if it never answers: the guest is untrusted. So each runs
//! on a thread of its own, never on tokio's blocking pool, which a guest's stuck calls would fill
//! and with it every other machine's work (machined's starts, other guests' calls); each machine
//! has at most [`MAX_CALLS`] under way, past which a call is refused with RESOURCE_EXHAUSTED; and a
//! file's read or write waits at most [`FILE_WAIT`] for any one answer. What still waits ends as
//! the machine stops: every agent call then fails at once.

use std::sync::Arc;
use std::time::Duration;

use libmachine::agent::Agent;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot};
use tonic::Status;

use crate::errors;

/// The most calls under way to one machine's guest (an exec counts until it is finished).
pub(crate) const MAX_CALLS: usize = 64;
/// The longest a file's read or write waits for any one answer from the guest.
const FILE_WAIT: Duration = Duration::from_secs(60);
/// A call's thread only waits: its stack needs little.
const THREAD_STACK: usize = 256 << 10;

/// A running machine's guest, to call.
#[derive(Clone)]
pub(crate) struct Guest {
    agent: Agent,
    calls: Arc<Semaphore>,
}

impl Guest {
    pub(crate) fn new(agent: Agent, calls: Arc<Semaphore>) -> Self {
        Self { agent, calls }
    }

    /// One of the machine's places for a call under way, or RESOURCE_EXHAUSTED.
    pub(crate) fn place(&self) -> Result<OwnedSemaphorePermit, Status> {
        self.calls.clone().try_acquire_owned().map_err(|_| {
            Status::resource_exhausted(format!(
                "{MAX_CALLS} calls to this machine's guest are under way, its most"
            ))
        })
    }

    /// Runs `f` with the guest's agent on a thread of its own, holding a place for it.
    pub(crate) async fn call<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Agent) -> libmachine::Result<T> + Send + 'static,
    ) -> Result<T, Status> {
        self.call_with(self.agent.clone(), f).await
    }

    /// As [`call`](Self::call), each of the agent's waits for the guest bounded by [`FILE_WAIT`].
    pub(crate) async fn file_call<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Agent) -> libmachine::Result<T> + Send + 'static,
    ) -> Result<T, Status> {
        self.call_with(self.agent.clone().timeout(Some(FILE_WAIT)), f).await
    }

    /// As [`call`](Self::call), each of the agent's waits bounded by `wait`.
    pub(crate) async fn call_waiting<T: Send + 'static>(
        &self,
        wait: Duration,
        f: impl FnOnce(&Agent) -> libmachine::Result<T> + Send + 'static,
    ) -> Result<T, Status> {
        self.call_with(self.agent.clone().timeout(Some(wait)), f).await
    }

    async fn call_with<T: Send + 'static>(
        &self,
        agent: Agent,
        f: impl FnOnce(&Agent) -> libmachine::Result<T> + Send + 'static,
    ) -> Result<T, Status> {
        let place = self.place()?;
        let (reply, answer) = oneshot::channel();
        spawn("machined-guest", move || {
            let _place = place;
            // A caller gone needs no answer: the call ran its course.
            let _ = reply.send(f(&agent));
        })?;
        match answer.await {
            Ok(result) => result.map_err(errors::status),
            Err(_) => Err(Status::internal("a guest call's thread panicked")),
        }
    }
}

/// Starts a thread of machined's guest calls.
pub(crate) fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> Result<(), Status> {
    std::thread::Builder::new()
        .name(name.into())
        .stack_size(THREAD_STACK)
        .spawn(f)
        .map(drop)
        .map_err(|e| Status::resource_exhausted(format!("a thread for a guest call: {e}")))
}
