// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The device's workers: a thread for each request queue the driver set up, which takes the
//! requests off it as the driver notifies the queue's event, and serves them, several at once
//! ([`Serve`], the device's [`Requests`](crate::Requests)), each returned to the driver on the
//! thread receiving the daemon's reply. The VMM gives each worker its queue, as a
//! [`RequestQueue`] of its transport's, the queue's event, and its interrupt
//! ([`QueueInterrupt`]); it takes the queues back as the workers stop, as they left them, for a
//! snapshot or a reset.
//!
//! A queue's event is a descriptor that is readable once the driver notifies the queue: an
//! eventfd, as KVM's ioeventfds are, or the read end of a pipe, as libmachine's events are on
//! macOS, which has no eventfd (board 13067). A worker waits on it and on its own stop event with
//! epoll on Linux, where Firecracker's seccomp filter allows its calls, and poll(2) elsewhere
//! ([`Watch`]).
//!
//! On Linux, threads start through `cladefs::thread`'s spawner, which the VMM sets: Firecracker's
//! starts them under its VMM thread's seccomp filter, rather than the vCPU's that activates the
//! device (TASK-50.1).

use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::sync::{Arc, Mutex};

use log::error;

use crate::BoxError;
use cladefs::extent_requests::Buffers;

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

/// What serves the requests a worker takes: the device's [`Requests`](crate::Requests), over its
/// cell; a fake in the workers' tests.
pub(crate) trait Serve: Send + Sync + 'static {
    /// Serves the request whose buffers are `buffers`, or that are no request's, and calls
    /// `complete` with the length the device wrote to it, on whichever thread finishes it.
    fn serve(self: Arc<Self>, buffers: Result<Buffers, BoxError>, complete: Completion);

    /// Waits until every request it was given is completed.
    fn wait_idle(&self);

    /// Logs what it served, as its worker stops.
    fn log_served(&self);
}

/// What returns a request to the driver, with the length the device wrote to it.
pub(crate) type Completion = Box<dyn FnOnce(u32) + Send>;

/// A queue a worker is to serve: its index among the device's, the queue, the event the driver
/// notifies it through (an eventfd, or a pipe's read end: see the module's docs), and its
/// interrupt.
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
    /// Starts a worker for each of `queues`, serving its requests with what `serve()` makes, each
    /// thread named `name` and the queue's index: all of them, or, one failing to start, none,
    /// those started stopped, and their queues lost with them. Left running, no stop or reset
    /// would reach them, and the next start would serve the same queues twice (TASK-79.44). A
    /// queue whose event poll can't wait on (always readable, as a regular file is) fails it
    /// (TASK-535).
    pub(crate) fn start(
        queues: Vec<WorkerQueue<Q>>,
        name: &str,
        mut serve: impl FnMut() -> Arc<dyn Serve>,
    ) -> io::Result<Self> {
        let mut running = Vec::with_capacity(queues.len());
        for queue in queues {
            match Worker::spawn(queue, name, serve()) {
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

/// What tells a worker to stop: on Linux an eventfd, which the worker waits on; elsewhere a
/// pipe, whose read end it waits on.
struct StopEvent {
    /// What the worker waits on.
    read: OwnedFd,
    /// What [`StopEvent::signal`] writes to: on Linux the eventfd itself.
    #[cfg(not(target_os = "linux"))]
    write: OwnedFd,
}

impl StopEvent {
    #[cfg(target_os = "linux")]
    fn new() -> io::Result<Self> {
        // SAFETY: A plain system call, whose result is checked.
        let fd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: A new descriptor, this one's alone.
        Ok(Self { read: unsafe { OwnedFd::from_raw_fd(fd) } })
    }

    // macOS has no pipe2: close-on-exec is set as the pipe is made, an instant later, as std's
    // own descriptors are there.
    #[cfg(not(target_os = "linux"))]
    fn new() -> io::Result<Self> {
        let mut fds = [0; 2];
        // SAFETY: `fds` has room for the two descriptors pipe writes; its result is checked.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: Two new descriptors, this one's alone.
        let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        for fd in [&read, &write] {
            // SAFETY: fcntl on a descriptor this owns; no pointers.
            if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(Self { read, write })
    }

    /// Tells the worker to stop: adds one to the eventfd's count, or a byte to the pipe.
    fn signal(&self) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        let (fd, one) = (self.read.as_raw_fd(), 1u64.to_ne_bytes());
        #[cfg(not(target_os = "linux"))]
        let (fd, one) = (self.write.as_raw_fd(), [1u8]);
        // SAFETY: Writes `one`'s bytes from a live local.
        if unsafe { libc::write(fd, one.as_ptr().cast(), one.len()) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

/// What a queue's event says, once it is readable: the driver notified the queue, or it can never
/// notify it again (a pipe whose every write end closed).
#[derive(Debug, PartialEq, Eq)]
enum Notified {
    Yes,
    Gone,
}

/// Takes what notified `fd`, an eventfd or a pipe's read end, readable: in one read, an eventfd's
/// whole count, or up to 64 of a pipe's bytes, any left keeping it readable for the next wait,
/// which takes the queue's requests once more, finding none. One read, so that a descriptor
/// opened blocking never blocks the worker.
fn drain(fd: BorrowedFd<'_>) -> io::Result<Notified> {
    let mut bytes = [0u8; 64];
    loop {
        // SAFETY: Reads into a live local buffer of its length.
        let read = unsafe { libc::read(fd.as_raw_fd(), bytes.as_mut_ptr().cast(), bytes.len()) };
        match read {
            0 => return Ok(Notified::Gone),
            read if read > 0 => return Ok(Notified::Yes),
            _ => {
                let err = io::Error::last_os_error();
                match err.kind() {
                    io::ErrorKind::Interrupted => {}
                    // Woken, and nothing to read: another reader of the event took it.
                    io::ErrorKind::WouldBlock => return Ok(Notified::Yes),
                    _ => return Err(err),
                }
            }
        }
    }
}

/// What a worker waits on, its queue's event and its stop event, made on the worker's thread. On
/// Linux, epoll, as the workers wait there under Firecracker's VMM seccomp filter, which allows
/// epoll's calls, an eventfd's and reads and writes (`vendor/firecracker/resources/seccomp`,
/// `resources/cladefs/seccomp`), not a pipe's nor fcntl's F_SETFL; elsewhere poll(2).
struct Watch {
    #[cfg(target_os = "linux")]
    epoll: OwnedFd,
    #[cfg(not(target_os = "linux"))]
    fds: [std::os::fd::RawFd; 2],
}

impl Watch {
    /// Watches `event` and `stop`, which outlive it: refusing an event it can't wait on, one
    /// always readable, as a regular file is (TASK-535): epoll refuses it on Linux; elsewhere a
    /// regular file's, a directory's or a block device's is refused here.
    fn new(event: BorrowedFd<'_>, stop: BorrowedFd<'_>) -> io::Result<Self> {
        #[cfg(target_os = "linux")]
        {
            // SAFETY: A plain system call, whose result is checked.
            let epoll = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
            if epoll < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: A new descriptor, this one's alone.
            let epoll = unsafe { OwnedFd::from_raw_fd(epoll) };
            for (i, fd) in [event, stop].into_iter().enumerate() {
                let mut watched = libc::epoll_event { events: libc::EPOLLIN as u32, u64: i as u64 };
                // SAFETY: Adds `fd`, open for as long as `epoll` is, with its event.
                let added = unsafe {
                    libc::epoll_ctl(
                        epoll.as_raw_fd(),
                        libc::EPOLL_CTL_ADD,
                        fd.as_raw_fd(),
                        &mut watched,
                    )
                };
                if added < 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(Self { epoll })
        }
        #[cfg(not(target_os = "linux"))]
        {
            // SAFETY: A zeroed stat is a valid one to fill.
            let mut stat: libc::stat = unsafe { std::mem::zeroed() };
            // SAFETY: Fills `stat` for an open descriptor.
            if unsafe { libc::fstat(event.as_raw_fd(), &mut stat) } < 0 {
                return Err(io::Error::last_os_error());
            }
            if matches!(stat.st_mode & libc::S_IFMT, libc::S_IFREG | libc::S_IFDIR | libc::S_IFBLK)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "a queue's event is a file poll finds always readable, not an event",
                ));
            }
            Ok(Self { fds: [event.as_raw_fd(), stop.as_raw_fd()] })
        }
    }

    /// Whether the event, and whether the stop event, is readable (or never will be: hung up,
    /// failed, closed), waiting for one.
    fn wait(&self) -> io::Result<[bool; 2]> {
        #[cfg(target_os = "linux")]
        {
            let mut events = [libc::epoll_event { events: 0, u64: 0 }; 2];
            let count = loop {
                // SAFETY: Fills at most `events.len()` events.
                let count =
                    unsafe { libc::epoll_wait(self.epoll.as_raw_fd(), events.as_mut_ptr(), 2, -1) };
                if count >= 0 {
                    break count as usize;
                }
                let err = io::Error::last_os_error();
                if err.kind() != io::ErrorKind::Interrupted {
                    return Err(err);
                }
            };
            let mut ready = [false; 2];
            for event in &events[..count] {
                if let Some(ready) = ready.get_mut(event.u64 as usize) {
                    *ready = true;
                }
            }
            Ok(ready)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let mut polled =
                self.fds.map(|fd| libc::pollfd { fd, events: libc::POLLIN, revents: 0 });
            loop {
                // SAFETY: `polled` is a live array of its length.
                let count =
                    unsafe { libc::poll(polled.as_mut_ptr(), polled.len() as libc::nfds_t, -1) };
                if count >= 0 {
                    break;
                }
                let err = io::Error::last_os_error();
                if err.kind() != io::ErrorKind::Interrupted {
                    return Err(err);
                }
            }
            // A descriptor that isn't open (POLLNVAL) counts too: its read fails, which ends the
            // worker, rather than spin it.
            let ready = libc::POLLIN | libc::POLLHUP | libc::POLLERR | libc::POLLNVAL;
            Ok(polled.map(|polled| polled.revents & ready != 0))
        }
    }
}

/// What a worker shares with the requests it serves.
struct Shared<Q> {
    queue: Mutex<Q>,
    interrupt: Arc<dyn QueueInterrupt>,
    serve: Arc<dyn Serve>,
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
    thread: Thread<Option<Q>>,
    stop: StopEvent,
}

impl<Q: RequestQueue> Worker<Q> {
    fn spawn(queue: WorkerQueue<Q>, name: &str, serve: Arc<dyn Serve>) -> io::Result<Self> {
        let WorkerQueue { index, queue, event, interrupt } = queue;
        let stop = StopEvent::new()?;
        let stop_theirs = stop.read.try_clone()?;
        let shared = Arc::new(Shared { queue: Mutex::new(queue), interrupt, serve });
        let (ready, started) = std::sync::mpsc::channel();
        let thread = spawn(format!("{name}{index}"), move || {
            // Made under the worker thread's confinement, which it waits under.
            let watch = match Watch::new(event.as_fd(), stop_theirs.as_fd()) {
                Ok(watch) => watch,
                Err(err) => {
                    // Its starter gone: nothing to tell.
                    let _ = ready.send(Err(err));
                    return None;
                }
            };
            // Its starter gone: nothing to tell.
            let _ = ready.send(Ok(()));
            Some(work(shared, &event, &watch))
        })?;
        started.recv().map_err(io::Error::other)??;
        Ok(Self { index, thread, stop })
    }

    /// Stops the worker once the requests it took are served, and returns the queue, as it left
    /// it: none if it panicked.
    fn stop(self) -> Option<Q> {
        if let Err(err) = self.stop.signal() {
            error!("extent device: stopping the worker: {err}");
        }
        let stopped = self.thread.join();
        if stopped.is_err() {
            error!("extent device: the worker panicked");
        }
        // Panicked, said above: nothing given back.
        stopped.ok().flatten()
    }
}

/// Serves the queue until its stop event is signalled, then waits for the requests it took to be
/// returned to the driver, and returns the queue as they left it.
fn work<Q: RequestQueue>(shared: Arc<Shared<Q>>, event: &OwnedFd, watch: &Watch) -> Q {
    take_requests(&shared, event, watch);
    shared.serve.wait_idle();
    shared.serve.log_served();
    // The requests returned, the last of them dropped its share.
    let shared = Arc::into_inner(shared).expect("the requests served hold the worker no more");
    shared.queue.into_inner().expect("Poisoned lock")
}

/// Takes the requests off the queue as the driver notifies `event`, and serves them, until the
/// stop event is signalled, or the event can notify no more.
fn take_requests<Q: RequestQueue>(shared: &Arc<Shared<Q>>, event: &OwnedFd, watch: &Watch) {
    loop {
        let [notified, stopped] = match watch.wait() {
            Ok(ready) => ready,
            Err(err) => {
                error!("extent worker: waiting for its events: {err}");
                return;
            }
        };
        if stopped {
            return;
        }
        if !notified {
            continue;
        }
        match drain(event.as_fd()) {
            Ok(Notified::Yes) => {}
            Ok(Notified::Gone) => {
                error!("extent worker: the queue's event can notify no more: the worker stops");
                return;
            }
            Err(err) => {
                error!("extent worker: reading the queue's event: {err}");
                return;
            }
        }
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
            let complete: Completion = Box::new(move |len| served.complete(index, len));
            shared.serve.clone().serve(buffers, complete);
        }
    }
}

/// A worker's thread: on Linux started through `cladefs::thread`'s spawner (the module's docs);
/// elsewhere, where cladefs confines no thread, as std starts it.
enum Thread<T> {
    #[cfg(target_os = "linux")]
    Cladefs(cladefs::thread::Thread<T>),
    #[cfg(not(target_os = "linux"))]
    Std(std::thread::JoinHandle<T>),
}

impl<T> Thread<T> {
    fn join(self) -> std::thread::Result<T> {
        match self {
            #[cfg(target_os = "linux")]
            Self::Cladefs(thread) => thread.join(),
            #[cfg(not(target_os = "linux"))]
            Self::Std(thread) => thread.join(),
        }
    }
}

fn spawn<T: Send + 'static>(
    name: String,
    run: impl FnOnce() -> T + Send + 'static,
) -> io::Result<Thread<T>> {
    #[cfg(target_os = "linux")]
    return cladefs::thread::spawn(name, run).map(Thread::Cladefs);
    #[cfg(not(target_os = "linux"))]
    return std::thread::Builder::new().name(name).spawn(run).map(Thread::Std);
}

#[cfg(test)]
mod tests {
    //! The workers over a fake server and queue, on pipe events, as libmachine's on macOS are
    //! (board 13067): what the daemon's tests (`tests/daemon.rs`, Linux's, on eventfds) can't
    //! reach. A pipe's notifications are bytes, several of which one drain takes; and a pipe
    //! whose writer is gone polls readable for good, which must end its worker rather than spin
    //! it.

    use std::collections::VecDeque;
    use std::sync::Condvar;
    use std::sync::mpsc;

    use super::*;

    /// A queue of the test's: the requests made available, by their heads, and those returned.
    struct FakeQueue {
        available: VecDeque<u16>,
        returned: mpsc::Sender<(u16, u32)>,
    }

    impl RequestQueue for FakeQueue {
        fn pop(&mut self) -> Result<Option<Popped>, BoxError> {
            Ok(self
                .available
                .pop_front()
                .map(|index| Popped { index, buffers: Ok(Buffers::new()) }))
        }

        fn complete(&mut self, index: u16, len: u32) -> Result<bool, BoxError> {
            self.returned.send((index, len)).unwrap();
            Ok(true)
        }
    }

    struct NoInterrupt;

    impl QueueInterrupt for NoInterrupt {
        fn notify(&self) -> Result<(), BoxError> {
            Ok(())
        }
    }

    /// Serves each request on a thread of its own, after `release` lets it: 7 bytes written. Says
    /// on `taken` as it is given each, and on `ended` when its worker is done.
    #[derive(Default)]
    struct Held {
        release: Mutex<bool>,
        released: Condvar,
        serving: Mutex<usize>,
        idle: Condvar,
        taken: Mutex<Option<mpsc::Sender<()>>>,
        ended: Mutex<Option<mpsc::Sender<()>>>,
    }

    impl Held {
        fn release(&self) {
            *self.release.lock().unwrap() = true;
            self.released.notify_all();
        }
    }

    impl Serve for Held {
        fn serve(self: Arc<Self>, _: Result<Buffers, BoxError>, complete: Completion) {
            *self.serving.lock().unwrap() += 1;
            if let Some(taken) = &*self.taken.lock().unwrap() {
                taken.send(()).unwrap();
            }
            std::thread::spawn(move || {
                drop(self.released.wait_while(self.release.lock().unwrap(), |go| !*go).unwrap());
                complete(7);
                let mut serving = self.serving.lock().unwrap();
                *serving -= 1;
                self.idle.notify_all();
            });
        }

        fn wait_idle(&self) {
            drop(self.idle.wait_while(self.serving.lock().unwrap(), |n| *n > 0).unwrap());
        }

        fn log_served(&self) {
            if let Some(ended) = self.ended.lock().unwrap().take() {
                ended.send(()).unwrap();
            }
        }
    }

    /// A pipe, as libmachine's events are on macOS: its read and write ends.
    fn pipe() -> (OwnedFd, OwnedFd) {
        let mut fds = [0; 2];
        // SAFETY: `fds` has room for the two descriptors pipe writes; its result is checked.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        // SAFETY: Two new descriptors, the test's alone.
        unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
    }

    fn notify(write: &OwnedFd, times: usize) {
        for _ in 0..times {
            // SAFETY: Writes one byte from a live local.
            assert_eq!(unsafe { libc::write(write.as_raw_fd(), [1u8].as_ptr().cast(), 1) }, 1);
        }
    }

    /// Notified through a pipe, three times before it looks, a worker takes every request made
    /// available once; stopped while one is still served, it waits for it to be returned, and
    /// gives the queue back as it left it.
    #[test]
    fn pipe_events_are_drained_and_stops_wait_for_requests_served() {
        let (read, write) = pipe();
        let (returned, returns) = mpsc::channel();
        let queue = FakeQueue { available: [3, 4].into(), returned };
        let (taken, took) = mpsc::channel();
        let held = Arc::new(Held { taken: Mutex::new(Some(taken)), ..Held::default() });
        let serve: Arc<dyn Serve> = held.clone();
        let queues =
            vec![WorkerQueue { index: 0, queue, event: read, interrupt: Arc::new(NoInterrupt) }];
        let workers = Workers::start(queues, "test_q", || serve.clone()).unwrap();
        notify(&write, 3);
        // Both taken on the notifications before the stop: a stop and a notification together
        // stop the worker, the notification left to the next start (as a VMM's resume kicks).
        took.recv().unwrap();
        took.recv().unwrap();
        let stopper = std::thread::spawn(move || workers.stop());
        // Not returned until released: the stop waits.
        assert_eq!(
            returns.recv_timeout(std::time::Duration::from_millis(50)),
            Err(mpsc::RecvTimeoutError::Timeout)
        );
        held.release();
        let mut got = [returns.recv().unwrap(), returns.recv().unwrap()];
        got.sort_unstable();
        assert_eq!(got, [(3, 7), (4, 7)]);
        let back = stopper.join().unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!((back[0].0, back[0].1.available.len()), (0, 0));
        // Each returned once, as the three notifications took them once.
        assert_eq!(returns.try_recv(), Err(mpsc::TryRecvError::Empty));
        assert_eq!(took.try_recv(), Err(mpsc::TryRecvError::Empty));
    }

    /// A worker whose pipe event's writer is gone stops on its own, saying so, rather than spin on
    /// a descriptor that polls readable for good; stopped then, it still gives its queue back.
    #[test]
    fn a_pipe_event_with_no_writer_ends_its_worker() {
        let (read, write) = pipe();
        let (returned, _returns) = mpsc::channel();
        let queue = FakeQueue { available: VecDeque::new(), returned };
        let (ended, worker_ended) = mpsc::channel();
        let serve: Arc<dyn Serve> =
            Arc::new(Held { ended: Mutex::new(Some(ended)), ..Held::default() });
        let queues =
            vec![WorkerQueue { index: 2, queue, event: read, interrupt: Arc::new(NoInterrupt) }];
        let workers = Workers::start(queues, "test_q", || serve.clone()).unwrap();
        drop(write);
        worker_ended.recv().unwrap();
        let back = workers.stop();
        assert_eq!(back.iter().map(|(index, _)| *index).collect::<Vec<_>>(), [2]);
    }
}
