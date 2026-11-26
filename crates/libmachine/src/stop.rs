// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Stopping a running machine from another thread.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::backend::kick::Kicker;
use crate::suspend::Doorbell;

/// Stops a [`Machine`](crate::Machine)'s [`run`](crate::Machine::run) from
/// any thread: `run` returns [`ExitReason::Stopped`](crate::ExitReason::Stopped)
/// promptly, with every vCPU stopped. A stop requested while the machine is
/// not running is kept, and the next `run` returns at once. The machine keeps
/// its state and can be run again.
///
/// Cheap to clone; every clone stops the same machine.
#[derive(Debug, Clone)]
pub struct StopHandle {
    shared: Arc<StopShared>,
}

#[derive(Debug, Default)]
pub(crate) struct StopShared {
    requested: AtomicBool,
    /// The machine's vCPU threads. Each registers when it starts and
    /// unregisters under this lock before it exits, so a thread that has
    /// exited is never kicked.
    threads: Mutex<Vec<Kicker>>,
    /// A suspended machine waits on it: a stop ends that wait too.
    doorbell: Arc<Doorbell>,
}

impl StopHandle {
    pub(crate) fn new(shared: Arc<StopShared>) -> Self {
        Self { shared }
    }

    /// Asks the machine to stop. Returns immediately; does not wait for `run`
    /// to return.
    pub fn stop(&self) {
        self.shared.requested.store(true, Ordering::SeqCst);
        self.shared.kick_all();
        self.shared.doorbell.stop_requested();
    }
}

impl StopShared {
    /// The stop state of a machine whose suspensions wait on `doorbell`.
    pub(crate) fn with_doorbell(doorbell: Arc<Doorbell>) -> Self {
        Self { doorbell, ..Self::default() }
    }

    fn lock_threads(&self) -> std::sync::MutexGuard<'_, Vec<Kicker>> {
        self.threads.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Consumes a pending stop request.
    pub(crate) fn take_request(&self) -> bool {
        self.requested.swap(false, Ordering::SeqCst)
    }

    /// Puts back a request [`take_request`](Self::take_request) consumed
    /// but that did not end a run, so the next run sees it.
    pub(crate) fn keep_request(&self) {
        self.requested.store(true, Ordering::SeqCst);
    }

    /// Kicks every vCPU out of the guest, or makes its next entry return at once.
    pub(crate) fn kick_all(&self) {
        for thread in self.lock_threads().iter() {
            thread.kick();
        }
    }

    /// Registers one of the machine's vCPU threads, by what kicks it, until the returned guard
    /// drops.
    pub(crate) fn register(&self, kicker: Kicker) -> Registration<'_> {
        self.lock_threads().push(kicker.clone());
        Registration(self, kicker)
    }
}

pub(crate) struct Registration<'a>(&'a StopShared, Kicker);

impl Drop for Registration<'_> {
    fn drop(&mut self) {
        self.0.lock_threads().retain(|t| *t != self.1);
    }
}
