// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The device thread: one per machine, it waits on every queue's
//! notification eventfd (the ioeventfds KVM signals when the guest writes a
//! queue's notify address) and has the device process the queue. A
//! notification therefore never exits to the vCPU thread, and the
//! interrupt that answers it goes out through an irqfd.

use std::os::fd::AsRawFd;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use vmm_sys_util::epoll::{ControlOperation, Epoll, EpollEvent, EventSet};
use vmm_sys_util::eventfd::{EFD_NONBLOCK, EventFd};

use super::pci::VirtioPci;
use crate::error::{Error, Result};

const STOP: u64 = u64::MAX;

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
        let epoll = Epoll::new().map_err(io_err("create the device thread's epoll"))?;
        let stop = EventFd::new(EFD_NONBLOCK)
            .map_err(io_err("create the device thread's stop eventfd"))?;
        let add = |fd, token| {
            epoll
                .ctl(ControlOperation::Add, fd, EpollEvent::new(EventSet::IN, token))
                .map_err(io_err("watch an eventfd"))
        };
        add(stop.as_raw_fd(), STOP)?;
        // Each source: its eventfd, its device and its queue (None for
        // the device's wake eventfd).
        let mut sources = Vec::new();
        for (d, device) in devices.iter().enumerate() {
            let device = device.lock().unwrap_or_else(|e| e.into_inner());
            for (q, evt) in device.notify_fds()?.into_iter().enumerate() {
                add(evt.as_raw_fd(), sources.len() as u64)?;
                sources.push((evt, d, Some(q)));
            }
            if let Some(evt) = device.wake_fd()? {
                add(evt.as_raw_fd(), sources.len() as u64)?;
                sources.push((evt, d, None));
            }
        }
        let handle = std::thread::Builder::new()
            .name("libmachine-devices".into())
            .spawn(move || run(&epoll, &sources, &devices))
            .map_err(io_err("start the device thread"))?;
        Ok(Self { stop, handle: Some(handle) })
    }
}

/// An eventfd the device thread waits on: its device, and its queue (None
/// for the device's wake eventfd).
type Source = (EventFd, usize, Option<usize>);

fn run(epoll: &Epoll, sources: &[Source], devices: &[Arc<Mutex<VirtioPci>>]) {
    let mut events = vec![EpollEvent::default(); 32];
    loop {
        let n = match epoll.wait(-1, &mut events) {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return,
        };
        for event in &events[..n] {
            let token = event.data();
            if token == STOP {
                return;
            }
            let (evt, d, q) = &sources[token as usize];
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
