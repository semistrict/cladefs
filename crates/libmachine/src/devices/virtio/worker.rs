// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The device thread: one per machine, it waits on every queue's
//! notification event (which the HVF backend signals when the guest writes a
//! queue's notify address) and has the device process the queue. A
//! notification therefore never makes the vCPU thread process a queue, and
//! the interrupt that answers it goes out through the HVF backend's irqfd
//! thread.

use std::os::fd::{AsRawFd, RawFd};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use super::pci::VirtioPci;
use crate::error::{Error, Result};
use crate::sys::{EFD_NONBLOCK, EventFd, wait_readable};

fn io_err(op: &'static str) -> impl FnOnce(std::io::Error) -> Error {
    move |source| Error::Hypervisor { op, source }
}

/// The running device thread; dropping it stops and joins the thread.
pub(crate) struct DeviceThread {
    stop: EventFd,
    handle: Option<JoinHandle<()>>,
}

impl DeviceThread {
    pub(crate) fn spawn(devices: Vec<Arc<Mutex<VirtioPci>>>) -> Result<Self> {
        let stop =
            EventFd::new(EFD_NONBLOCK).map_err(io_err("create the device thread's stop event"))?;
        // Each source: its event, its device and its queue (None for the
        // device's wake event).
        let mut sources = Vec::new();
        for (d, device) in devices.iter().enumerate() {
            let device = device.lock().unwrap_or_else(|e| e.into_inner());
            for (q, evt) in device.notify_fds()?.into_iter().enumerate() {
                sources.push((evt, d, Some(q)));
            }
            if let Some(evt) = device.wake_fd()? {
                sources.push((evt, d, None));
            }
        }
        let theirs = stop.try_clone().map_err(io_err("clone the device thread's stop event"))?;
        let handle = std::thread::Builder::new()
            .name("libmachine-devices".into())
            .spawn(move || run(&theirs, &sources, &devices))
            .map_err(io_err("start the device thread"))?;
        Ok(Self { stop, handle: Some(handle) })
    }
}

/// An event the device thread waits on: its device, and its queue (None
/// for the device's wake event).
type Source = (EventFd, usize, Option<usize>);

fn run(stop: &EventFd, sources: &[Source], devices: &[Arc<Mutex<VirtioPci>>]) {
    // The stop event last, after the sources, so that their indices are theirs.
    let fds: Vec<RawFd> =
        sources.iter().map(|(evt, _, _)| evt.as_raw_fd()).chain([stop.as_raw_fd()]).collect();
    loop {
        let ready = match wait_readable(&fds, -1) {
            Ok(ready) => ready,
            Err(_) => return,
        };
        for i in ready {
            let Some((evt, d, q)) = sources.get(i) else {
                return;
            };
            // Several notifications coalesce into one read: the device
            // takes everything available each time.
            if evt.read().is_ok() {
                let mut device = devices[*d].lock().unwrap_or_else(|e| e.into_inner());
                match q {
                    Some(q) => device.notified(*q),
                    None => device.woken(),
                }
            }
        }
    }
}

impl Drop for DeviceThread {
    fn drop(&mut self) {
        let _ = self.stop.write(1);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}
