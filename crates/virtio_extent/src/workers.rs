// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The device's workers: a thread for each request queue the driver set up, which takes the
//! requests off it as the driver notifies the queue's event, and serves them, several at once
//! ([`Requests`]), each returned to the driver on the thread receiving the daemon's reply. The
//! VMM gives each worker its queue, as a [`RequestQueue`] of its transport's, the queue's event,
//! and its interrupt ([`QueueInterrupt`]); it takes the queues back as the workers stop, as they
//! left them, for a snapshot or a reset.
//!
//! Threads start through `cladefs::thread`'s spawner, which the VMM sets: Firecracker's starts them
//! under its VMM thread's seccomp filter, rather than the vCPU's that activates the device
//! (TASK-50.1).

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::{Arc, Mutex};

use log::error;

use crate::BoxError;
use crate::buffers::Buffers;
use crate::requests::Requests;

/// A request the driver made available: its descriptor chain's head, and its buffers, or why the
/// chain is no request's (its guest memory not the guest's, or too long: [`BuffersError`]),
/// which the device returns to the driver with nothing written.
///
/// [`BuffersError`]: crate::BuffersError
pub struct Popped {
    pub index: u16,
    pub buffers: Result<Buffers, BoxError>,
}

/// A request queue of the VMM's transport, which a worker takes requests off and returns them to.
pub trait RequestQueue: Send + 'static {
    /// The next request the driver made available, if any. With notification suppression (event
    /// idx), one that finds the queue empty asks the driver to notify again, and looks once more,
    /// so that nothing made available meanwhile goes unnoticed. Each descriptor's guest memory is
    /// pushed to the request's [`Buffers`] in the chain's order, writable ones marked dirty if the
    /// VMM tracks them; its safety is the VMM's: mapped until the request is returned.
    fn pop(&mut self) -> Result<Option<Popped>, BoxError>;

    /// Returns the request whose chain's head is `index` to the driver, the device having written
    /// `len` bytes of it: whether the driver is to be interrupted (with event idx, as it asked).
    fn complete(&mut self, index: u16, len: u32) -> Result<bool, BoxError>;
}

/// A queue's interrupt: its MSI-X vector, which tells the driver of requests returned.
pub trait QueueInterrupt: Send + Sync {
    fn notify(&self) -> Result<(), BoxError>;
}

/// A queue a worker is to serve: its index among the device's, the queue, the event the driver
/// notifies it through (an eventfd), and its interrupt.
pub struct WorkerQueue<Q> {
    pub index: u16,
    pub queue: Q,
    pub event: OwnedFd,
    pub interrupt: Arc<dyn QueueInterrupt>,
}

/// The workers serving a device's queues, which return them as they stop.
pub struct Workers<Q: RequestQueue> {
    running: Vec<Worker<Q>>,
}

impl<Q: RequestQueue> std::fmt::Debug for Workers<Q> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let queues: Vec<u16> = self.running.iter().map(|worker| worker.index).collect();
        f.debug_struct("Workers").field("queues", &queues).finish()
    }
}

impl<Q: RequestQueue> Workers<Q> {
    /// Starts a worker for each of `queues`, serving its requests with `requests(index)`, each
    /// thread named `name` and the queue's index: all of them, or, one failing to start, none,
    /// those started stopped, and their queues lost with them. Left running, no stop or reset
    /// would reach them, and the next start would serve the same queues twice (TASK-79.44).
    pub(crate) fn start(
        queues: Vec<WorkerQueue<Q>>,
        name: &str,
        mut requests: impl FnMut() -> Requests,
    ) -> io::Result<Self> {
        let mut running = Vec::with_capacity(queues.len());
        for queue in queues {
            match Worker::spawn(queue, name, Arc::new(requests())) {
                Ok(worker) => running.push(worker),
                Err(err) => {
                    Self { running }.stop();
                    return Err(err);
                }
            }
        }
        Ok(Self { running })
    }

    /// Stops the workers once the requests they took are served, and returns their queues, by
    /// index, as they left them: none of one whose worker panicked.
    pub fn stop(self) -> Vec<(u16, Q)> {
        self.running.into_iter().filter_map(|worker| Some((worker.index, worker.stop()?))).collect()
    }
}

/// An eventfd of the worker's own: what tells it to stop.
fn eventfd() -> io::Result<OwnedFd> {
    // SAFETY: A plain system call, whose result is checked.
    let fd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: A new descriptor, this one's alone.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Reads an eventfd's count, resetting it; nothing if it is 0, as it is nonblocking.
fn drain(fd: RawFd) {
    let mut count = 0u64;
    // SAFETY: Reads 8 bytes into `count`.
    let _ = unsafe { libc::read(fd, (&raw mut count).cast(), size_of::<u64>()) };
}

/// Adds one to an eventfd's count.
fn signal(fd: RawFd) -> io::Result<()> {
    let one = 1u64;
    // SAFETY: Writes `one`'s 8 bytes.
    if unsafe { libc::write(fd, (&raw const one).cast(), size_of::<u64>()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// What a worker shares with the requests it serves.
struct Shared<Q> {
    queue: Mutex<Q>,
    interrupt: Arc<dyn QueueInterrupt>,
    requests: Arc<Requests>,
}

impl<Q: RequestQueue> Shared<Q> {
    /// Returns the request at `index` to the driver, with `len` bytes written to it.
    fn complete(&self, index: u16, len: u32) {
        let notify = match self.queue.lock().expect("Poisoned lock").complete(index, len) {
            Ok(notify) => notify,
            Err(err) => {
                error!("extent worker: adding a used element: {err}");
                return;
            }
        };
        if notify && let Err(err) = self.interrupt.notify() {
            error!("extent worker: signalling the queue: {err}");
        }
    }
}

/// A worker's thread, once spawned, which stops when told to.
struct Worker<Q> {
    index: u16,
    /// Returns the queue, as the worker left it.
    thread: cladefs::thread::Thread<Option<Q>>,
    stop: OwnedFd,
}

impl<Q: RequestQueue> Worker<Q> {
    fn spawn(queue: WorkerQueue<Q>, name: &str, requests: Arc<Requests>) -> io::Result<Self> {
        let WorkerQueue { index, queue, event, interrupt } = queue;
        let stop = eventfd()?;
        let stop_theirs = stop.try_clone()?;
        let shared = Arc::new(Shared { queue: Mutex::new(queue), interrupt, requests });
        let (ready, started) = std::sync::mpsc::channel();
        let thread = cladefs::thread::spawn(format!("{name}{index}"), move || {
            let epoll = match watch(&event, &stop_theirs) {
                Ok(epoll) => epoll,
                Err(err) => {
                    let _ = ready.send(Err(err));
                    return None;
                }
            };
            let _ = ready.send(Ok(()));
            Some(work(shared, &event, &stop_theirs, &epoll))
        })?;
        started.recv().map_err(io::Error::other)??;
        Ok(Self { index, thread, stop })
    }

    /// Stops the worker once the requests it took are served, and returns the queue, as it left
    /// it: none if it panicked.
    fn stop(self) -> Option<Q> {
        if let Err(err) = signal(self.stop.as_raw_fd()) {
            error!("extent device: stopping the worker: {err}");
        }
        let stopped = self.thread.join();
        if stopped.is_err() {
            error!("extent device: the worker panicked");
        }
        stopped.ok().flatten()
    }
}

/// Serves the queue until `stop` is signalled, then waits for the requests it took to be returned
/// to the driver, and returns the queue as they left it.
fn work<Q: RequestQueue>(
    shared: Arc<Shared<Q>>,
    event: &OwnedFd,
    stop: &OwnedFd,
    epoll: &OwnedFd,
) -> Q {
    take_requests(&shared, event, stop, epoll);
    shared.requests.wait_idle();
    shared.requests.served().log();
    // The requests returned, the last of them dropped its share.
    let shared = Arc::into_inner(shared).expect("the requests served hold the worker no more");
    shared.queue.into_inner().expect("Poisoned lock")
}

/// Registers both descriptors under the worker thread's confinement.
fn watch(event: &OwnedFd, stop: &OwnedFd) -> io::Result<OwnedFd> {
    // SAFETY: A plain system call, whose result is checked.
    let epoll = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
    if epoll < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: A new descriptor, this one's alone.
    let epoll = unsafe { OwnedFd::from_raw_fd(epoll) };
    let (event_fd, stop_fd) = (event.as_raw_fd(), stop.as_raw_fd());
    for fd in [event_fd, stop_fd] {
        let mut watched = libc::epoll_event { events: libc::EPOLLIN as u32, u64: fd as u64 };
        // SAFETY: Adds `fd`, open for as long as `epoll` is, with its event.
        if unsafe { libc::epoll_ctl(epoll.as_raw_fd(), libc::EPOLL_CTL_ADD, fd, &mut watched) } < 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(epoll)
}

/// Takes the requests off the queue as the driver notifies `event`, and serves them, until `stop`
/// is signalled.
fn take_requests<Q: RequestQueue>(
    shared: &Arc<Shared<Q>>,
    event: &OwnedFd,
    stop: &OwnedFd,
    epoll: &OwnedFd,
) {
    let (event_fd, stop_fd) = (event.as_raw_fd(), stop.as_raw_fd());
    let mut events = [libc::epoll_event { events: 0, u64: 0 }; 2];
    loop {
        // SAFETY: Fills at most `events.len()` events.
        let count = unsafe { libc::epoll_wait(epoll.as_raw_fd(), events.as_mut_ptr(), 2, -1) };
        if count < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            error!("extent worker: epoll: {err}");
            return;
        }
        for event in &events[..count as usize] {
            let fd = event.u64 as RawFd;
            if fd == stop_fd {
                drain(stop_fd);
                return;
            }
            if fd != event_fd {
                continue;
            }
            drain(event_fd);
            let mut taken = Vec::new();
            {
                let mut queue = shared.queue.lock().expect("Poisoned lock");
                loop {
                    match queue.pop() {
                        Ok(Some(popped)) => taken.push(popped),
                        Ok(None) => break,
                        Err(err) => {
                            error!("extent worker: {err}");
                            break;
                        }
                    }
                }
            }
            for Popped { index, buffers } in taken {
                let served = shared.clone();
                shared.requests.serve(buffers, move |len| served.complete(index, len));
            }
        }
    }
}
