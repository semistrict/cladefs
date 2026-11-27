// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Stopping a running machine from another thread.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::backend::kick::Kicker;

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
    /// Why a device stopped the machine for good ([`StopShared::fail`]): every run from then on
    /// ends at once with it. Read before every entry into the guest, so without a lock.
    failed: std::sync::OnceLock<Failure>,
}

/// Why a device stopped its machine for good.
#[derive(Debug, Clone)]
pub(crate) enum Failure {
    /// Its extent daemon went ([`crate::Error::ExtentDaemonLost`]).
    DaemonLost(String),
    /// Anything else ([`crate::Error::ExtentStore`]).
    Other(String),
}

impl From<Failure> for crate::Error {
    fn from(failure: Failure) -> Self {
        match failure {
            Failure::DaemonLost(why) => Self::ExtentDaemonLost(why),
            Failure::Other(why) => Self::ExtentStore(why),
        }
    }
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
    }
}

impl StopShared {
    fn lock_threads(&self) -> std::sync::MutexGuard<'_, Vec<Kicker>> {
        self.threads.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Stops the machine for good, as a device that can't go on does (an extent store device
    /// whose extent daemon went, or whose guest's accesses cladefs refuses): the run ends with
    /// the error `why` says ([`Failure`]), as does every run after it. The first reason given is
    /// the one told.
    pub(crate) fn fail(&self, why: Failure) {
        // A reason given already is the one told: this one is dropped.
        let _ = self.failed.set(why);
        self.requested.store(true, Ordering::SeqCst);
        self.kick_all();
    }

    /// Why a device stopped the machine for good, if one did ([`StopShared::fail`]).
    pub(crate) fn failure(&self) -> Option<crate::Error> {
        self.failed.get().cloned().map(crate::Error::from)
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
