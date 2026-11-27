// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! irqfds, in the VMM: a thread per VM waits on every interrupt event a device signals (an
//! MSI-X vector's), and sends the MSI its GSI's route names to the GIC. The devices signal an
//! event, and an interrupt reaches the guest.

use std::os::fd::{AsRawFd, RawFd};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;

use super::Shared;
use crate::error::{Error, Result};
use crate::sys::{EFD_NONBLOCK, EventFd, wait_readable};

fn io_err(op: &'static str) -> impl FnOnce(std::io::Error) -> Error {
    move |source| Error::Hypervisor { op, source }
}

/// The events and the GSIs they raise.
#[derive(Default)]
struct Table {
    assigned: Vec<(EventFd, u32)>,
    /// The thread is to exit.
    quit: bool,
}

/// The irqfd thread; [`stop`](Forwarder::stop) (or dropping it) ends it.
pub(crate) struct Forwarder {
    table: Arc<Mutex<Table>>,
    /// Wakes the thread to read the table again.
    changed: EventFd,
    handle: Option<JoinHandle<()>>,
}

fn lock(table: &Mutex<Table>) -> MutexGuard<'_, Table> {
    table.lock().unwrap_or_else(|e| e.into_inner())
}

impl Forwarder {
    pub(crate) fn spawn(shared: Arc<Shared>) -> Result<Self> {
        let table = Arc::new(Mutex::new(Table::default()));
        let changed =
            EventFd::new(EFD_NONBLOCK).map_err(io_err("create the irqfd thread's event"))?;
        let (theirs, their_changed) =
            (table.clone(), changed.try_clone().map_err(io_err("clone the irqfd thread's event"))?);
        let handle = std::thread::Builder::new()
            .name("libmachine-irqfd".into())
            .spawn(move || run(&theirs, &their_changed, &shared))
            .map_err(io_err("start the irqfd thread"))?;
        Ok(Self { table, changed, handle: Some(handle) })
    }

    /// Makes a signal on `evt` send `gsi`'s MSI.
    pub(crate) fn assign(&self, evt: &EventFd, gsi: u32) -> Result<()> {
        let evt = evt.try_clone().map_err(io_err("clone an irqfd"))?;
        lock(&self.table).assigned.push((evt, gsi));
        self.changed.write(1).map_err(io_err("wake the irqfd thread"))
    }

    pub(crate) fn deassign(&self, evt: &EventFd, gsi: u32) -> Result<()> {
        lock(&self.table)
            .assigned
            .retain(|(e, g)| !(*g == gsi && e.as_raw_fd() == evt.as_raw_fd()));
        self.changed.write(1).map_err(io_err("wake the irqfd thread"))
    }

    /// Ends the thread and waits for it.
    pub(crate) fn stop(&mut self) {
        lock(&self.table).quit = true;
        let _ = self.changed.write(1);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for Forwarder {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run(table: &Mutex<Table>, changed: &EventFd, shared: &Shared) {
    loop {
        // The table as of now: its descriptors, then the change event's, last.
        let (fds, gsis): (Vec<RawFd>, Vec<u32>) = {
            let table = lock(table);
            if table.quit {
                return;
            }
            table.assigned.iter().map(|(e, g)| (e.as_raw_fd(), *g)).unzip()
        };
        let all: Vec<RawFd> = fds.iter().copied().chain([changed.as_raw_fd()]).collect();
        let Ok(ready) = wait_readable(&all, -1) else {
            return;
        };
        for i in ready {
            if i == fds.len() {
                let _ = changed.read();
                continue;
            }
            // The event may have been deassigned since: read it through the table.
            let fired = lock(table)
                .assigned
                .iter()
                .any(|(e, g)| e.as_raw_fd() == fds[i] && *g == gsis[i] && e.read().is_ok());
            if fired {
                shared.send_msi(gsis[i]);
            }
        }
    }
}
