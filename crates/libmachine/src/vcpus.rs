// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A machine's vCPU threads, and the runs they make together.
//!
//! Every vCPU has a host thread of its own for the machine's whole life:
//! KVM, seccomp filters and cladefs's per-thread setup all want a vCPU to stay
//! on one thread (KVM pays a `synchronize_rcu` whenever a vCPU's `KVM_RUN`
//! comes from a new thread). Between runs the threads wait. A run starts
//! them all; the caller's thread runs no vCPU, it waits for the run to end.
//!
//! A run ends with one outcome: the first of a vCPU's exit (power-off,
//! reset, triple fault, an error, a panic), a stop request, or the timeout.
//! Whoever records it then stops the others: it sets `stopping` and kicks
//! every vCPU thread out of `KVM_RUN`. Outcomes that come later are
//! dropped, except a stop request, which is kept for the next run. The run
//! returns once every vCPU has stopped, so the machine's state is still
//! between runs.
//!
//! The caller may also wait in steps ([`Vcpus::start`], then
//! [`Vcpus::wait`] with a limit, as often as it likes): a wait that reaches
//! its limit returns with the vCPUs still running, undisturbed. The run's
//! own timeout is enforced by whoever waits.
//!
//! Between runs, work can be sent to every vCPU's own thread ([`Vcpus::on_each`]):
//! Hypervisor.framework lets only the thread that created a vCPU read or set its state, so a
//! snapshot and a restore go through there.
//!
//! The coordination is generic over the vCPU loop ([`VcpuLoop`]), so that
//! its unit tests run fake vCPUs without KVM.

use std::ops::ControlFlow;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, mpsc};
use std::thread::JoinHandle;
#[cfg(test)]
use std::time::Duration;
use std::time::Instant;

use crate::backend::kick::{KickTarget, VcpuThreadScope};
use crate::error::{Error, Result};
use crate::machine::ExitReason;
use crate::stop::StopShared;

/// What a vCPU thread runs.
pub(crate) trait VcpuLoop: Send + 'static {
    /// What the kick needs to reach this vCPU (`backend::kick`): on KVM, the `immediate_exit`
    /// byte the kick signal sets, which must stay valid as long as the value lives.
    fn kick_target(&mut self) -> KickTarget;

    /// Sets up the vCPU's thread, once, before the vCPU first runs on it.
    fn thread_started(&mut self) {}

    /// Undoes [`thread_started`](Self::thread_started) as the thread exits, the machine dropped.
    fn thread_ended(&mut self) {}

    /// Runs the vCPU until it ends the run (`Some(outcome)`), or until
    /// [`RunControl::check`], which it calls before every entry into the
    /// guest, says to stop.
    fn run(&mut self, ctl: &RunControl<'_>) -> Option<Result<ExitReason>>;
}

/// What a running vCPU asks before it enters the guest.
pub(crate) struct RunControl<'a> {
    shared: &'a Shared,
}

impl RunControl<'_> {
    /// Whether the vCPU should stop instead of entering the guest:
    /// `Break(None)` when the machine is stopping, `Break(Some(Stopped))`
    /// when this vCPU is the first to see a stop request.
    ///
    /// The kick sets `immediate_exit` after the flags it signals, so a vCPU
    /// that clears `immediate_exit` before calling this never loses a kick.
    pub(crate) fn check(&self) -> ControlFlow<Option<Result<ExitReason>>> {
        if self.shared.stopping.load(Ordering::SeqCst) {
            return ControlFlow::Break(None);
        }
        if let Some(failure) = self.shared.stop.failure() {
            return ControlFlow::Break(Some(Err(failure)));
        }
        if self.shared.stop.take_request() {
            return ControlFlow::Break(Some(Ok(ExitReason::Stopped)));
        }
        ControlFlow::Continue(())
    }
}

struct Shared {
    state: Mutex<State>,
    /// vCPU threads wait on it for a run (or the end).
    start: Condvar,
    /// The caller waits on it for the vCPUs to stop.
    parked: Condvar,
    /// The run is ending: vCPUs stop before entering the guest again.
    stopping: AtomicBool,
    stop: Arc<StopShared>,
}

/// Work for one vCPU's thread, between runs ([`Vcpus::on_each`]).
type Job = Box<dyn FnOnce() + Send>;

/// What a vCPU's thread does with its vCPU, between runs, for [`Vcpus::on_each`].
pub(crate) type VcpuJob<V, R> = Box<dyn FnOnce(&mut V) -> Result<R> + Send>;

#[derive(Default)]
struct State {
    /// Counts runs; a vCPU thread runs once for each.
    generation: u64,
    /// The machine is dropped: threads exit.
    quit: bool,
    /// Work waiting for each vCPU's thread, by vCPU.
    jobs: Vec<Option<Job>>,
    /// vCPU threads still in the current run.
    running: usize,
    /// The run's outcome, and the vCPU that reported it (none for a
    /// timeout).
    outcome: Option<(Result<ExitReason>, Option<u32>)>,
}

fn lock<T: ?Sized>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    /// Ends the run: every vCPU stops before it enters the guest again.
    fn stop_all(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        self.stop.kick_all();
    }

    /// A vCPU ends the run with `outcome`, unless the run already has one.
    fn finish(&self, outcome: Result<ExitReason>, vcpu: u32) {
        let mut state = self.state();
        if state.outcome.is_none() {
            state.outcome = Some((outcome, Some(vcpu)));
        } else if matches!(outcome, Ok(ExitReason::Stopped)) {
            // This vCPU consumed a stop request that did not end the run:
            // keep it for the next one, as for a request made between runs.
            self.stop.keep_request();
        }
        drop(state);
        self.stop_all();
    }
}

/// The vCPU threads of one machine.
pub(crate) struct Vcpus<V: VcpuLoop> {
    shared: Arc<Shared>,
    vcpus: Vec<Arc<Mutex<V>>>,
    threads: Vec<JoinHandle<()>>,
    /// The vCPU that ended the last run, if one did.
    last_vcpu: Option<u32>,
    /// A run is in progress: started, its outcome not yet returned.
    in_run: bool,
    /// The run in progress's timeout, until it is enforced.
    deadline: Option<Instant>,
}

impl<V: VcpuLoop> Vcpus<V> {
    /// Starts a thread for each of `vcpus`, which wait for [`run`].
    ///
    /// [`run`]: Vcpus::run
    pub(crate) fn spawn(vcpus: Vec<V>, stop: Arc<StopShared>) -> Result<Self> {
        let jobs = vcpus.iter().map(|_| None).collect();
        let mut this = Self {
            shared: Arc::new(Shared {
                state: Mutex::new(State { jobs, ..State::default() }),
                start: Condvar::new(),
                parked: Condvar::new(),
                stopping: AtomicBool::new(false),
                stop,
            }),
            vcpus: Vec::with_capacity(vcpus.len()),
            threads: Vec::with_capacity(vcpus.len()),
            last_vcpu: None,
            in_run: false,
            deadline: None,
        };
        for (id, vcpu) in vcpus.into_iter().enumerate() {
            let id = id as u32;
            let vcpu = Arc::new(Mutex::new(vcpu));
            let (ready_tx, ready_rx) = mpsc::sync_channel(1);
            let (thread_vcpu, shared) = (vcpu.clone(), this.shared.clone());
            let thread = std::thread::Builder::new()
                .name(format!("vcpu{id}"))
                .spawn(move || vcpu_thread(id, thread_vcpu, shared, ready_tx))
                .map_err(|source| Error::Hypervisor { op: "start a vCPU thread", source })?;
            this.vcpus.push(vcpu);
            this.threads.push(thread);
            // A thread that cannot set itself up exits after saying why;
            // dropping `this` joins the ones started so far.
            ready_rx.recv().unwrap_or_else(|_| {
                Err(Error::Hypervisor {
                    op: "start a vCPU thread",
                    source: std::io::Error::other("the thread exited during setup"),
                })
            })?;
        }
        Ok(this)
    }

    /// How many vCPUs there are.
    pub(crate) fn len(&self) -> usize {
        self.vcpus.len()
    }

    /// vCPU `id`, between runs.
    pub(crate) fn get(&self, id: usize) -> MutexGuard<'_, V> {
        lock(&self.vcpus[id])
    }

    /// Runs `jobs[i]` on vCPU `i`'s own thread, every vCPU's at once, between runs, and returns
    /// what each returned, in vCPU order: the first error, once every job has ended. One job per
    /// vCPU. A job that panics is an error.
    #[cfg_attr(
        not(all(target_os = "macos", target_arch = "aarch64")),
        allow(dead_code, reason = "KVM sets a vCPU's state from any thread")
    )]
    pub(crate) fn on_each<R: Send + 'static>(&self, jobs: Vec<VcpuJob<V, R>>) -> Result<Vec<R>> {
        assert_eq!(jobs.len(), self.vcpus.len(), "a job per vCPU");
        self.run_jobs(jobs.into_iter().enumerate().collect())
    }

    /// Runs `job` on vCPU `id`'s own thread, between runs, as [`on_each`](Self::on_each) does.
    #[cfg_attr(
        not(all(target_os = "macos", target_arch = "aarch64")),
        allow(dead_code, reason = "KVM sets a vCPU's state from any thread")
    )]
    pub(crate) fn on<R: Send + 'static>(&self, id: usize, job: VcpuJob<V, R>) -> Result<R> {
        let mut done = self.run_jobs(vec![(id, job)])?;
        Ok(done.pop().expect("one job, one answer"))
    }

    /// Runs each job on its vCPU's thread, at once: their answers in their order.
    fn run_jobs<R: Send + 'static>(&self, jobs: Vec<(usize, VcpuJob<V, R>)>) -> Result<Vec<R>> {
        assert!(!self.in_run, "vCPU jobs run between runs");
        let (tx, rx) = mpsc::channel();
        let count = jobs.len();
        {
            let mut state = self.shared.state();
            for (order, (id, job)) in jobs.into_iter().enumerate() {
                let (vcpu, tx) = (self.vcpus[id].clone(), tx.clone());
                let job: Job = Box::new(move || {
                    let done = catch_unwind(AssertUnwindSafe(|| job(&mut lock(&vcpu))))
                        .unwrap_or_else(|panic| Err(panicked(id as u32, &*panic)));
                    // The caller waits for every job's answer.
                    let _ = tx.send((order, done));
                });
                assert!(state.jobs[id].is_none(), "one job of a vCPU's at a time");
                state.jobs[id] = Some(job);
            }
            self.shared.start.notify_all();
        }
        drop(tx);
        let mut done: Vec<Option<Result<R>>> = (0..count).map(|_| None).collect();
        // Each job answers once, as its thread runs it; a thread runs every job it is given
        // before it quits, and quits only when the machine drops, after this returns.
        for (order, outcome) in rx {
            done[order] = Some(outcome);
        }
        done.into_iter()
            .map(|outcome| outcome.expect("every vCPU thread answers its job"))
            .collect()
    }

    /// The vCPU that ended the last run (`None` for a timeout, a stop
    /// request seen before the run started, or before any run).
    #[cfg_attr(not(test), allow(dead_code, reason = "diagnostics, read by tests"))]
    pub(crate) fn last_vcpu(&self) -> Option<u32> {
        self.last_vcpu
    }

    /// The CPU time each vCPU thread has used.
    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn thread_cpu_times(&self) -> Vec<Duration> {
        use std::os::unix::thread::JoinHandleExt;
        self.threads
            .iter()
            .map(|t| {
                let mut clock: libc::clockid_t = 0;
                let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
                // SAFETY: the thread is alive (joined only on drop); valid
                // pointers to owned values.
                unsafe {
                    assert_eq!(
                        libc::pthread_getcpuclockid(
                            t.as_pthread_t() as libc::pthread_t,
                            &mut clock
                        ),
                        0
                    );
                    assert_eq!(libc::clock_gettime(clock, &mut ts), 0);
                }
                Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
            })
            .collect()
    }

    /// Runs every vCPU until one ends the run, a stop request, or
    /// `deadline`; returns once all have stopped.
    // Tests' shorthand: machines always run through `Machine::start_run`'s
    // `Run`, whose every outcome passes the machine's after-run hook.
    #[cfg(test)]
    pub(crate) fn run(&mut self, deadline: Option<Instant>) -> Result<ExitReason> {
        match self.start(deadline) {
            Some(outcome) => outcome,
            None => self.wait(None).expect("a wait without a limit lasts the run"),
        }
    }

    /// Whether a run is in progress: [`start`](Vcpus::start)ed, and its
    /// outcome not yet returned by [`wait`](Vcpus::wait).
    pub(crate) fn in_run(&self) -> bool {
        self.in_run
    }

    /// Starts a run of every vCPU, which ends at the first vCPU outcome, a
    /// stop request, [`stop`](Vcpus::stop) or `deadline`. Returns the
    /// outcome at once if the run ends before any vCPU enters the guest (a
    /// stop requested between runs, a deadline already past); otherwise the
    /// run is in progress and [`wait`](Vcpus::wait) returns its outcome.
    pub(crate) fn start(&mut self, deadline: Option<Instant>) -> Option<Result<ExitReason>> {
        assert!(!self.in_run, "a run is already in progress");
        self.last_vcpu = None;
        if let Some(failure) = self.shared.stop.failure() {
            return Some(Err(failure));
        }
        if self.shared.stop.take_request() {
            return Some(Ok(ExitReason::Stopped));
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return Some(Ok(ExitReason::TimedOut));
        }
        let shared = &*self.shared;
        let mut state = shared.state();
        state.outcome = None;
        state.running = self.threads.len();
        state.generation += 1;
        shared.stopping.store(false, Ordering::SeqCst);
        shared.start.notify_all();
        drop(state);
        self.in_run = true;
        self.deadline = deadline;
        None
    }

    /// Waits for the run in progress to end, enforcing its deadline, until
    /// `until`: returns its outcome once every vCPU has stopped, or `None`,
    /// with the vCPUs still running and never kicked, if the run has no
    /// outcome by `until`. A run that has its outcome is waited for to the
    /// end whatever `until` (its vCPUs are only stopping).
    pub(crate) fn wait(&mut self, until: Option<Instant>) -> Option<Result<ExitReason>> {
        assert!(self.in_run, "no run in progress");
        let shared = self.shared.clone();
        let mut state = shared.state();
        loop {
            if state.running == 0 {
                let (outcome, vcpu) =
                    state.outcome.take().expect("vCPUs stop only once the run has an outcome");
                self.last_vcpu = vcpu;
                self.in_run = false;
                self.deadline = None;
                return Some(outcome);
            }
            let now = Instant::now();
            if self.deadline.is_some_and(|d| now >= d) {
                if state.outcome.is_none() {
                    state.outcome = Some((Ok(ExitReason::TimedOut), None));
                }
                shared.stop_all();
                self.deadline = None;
                continue;
            }
            let until = until.filter(|_| state.outcome.is_none());
            if until.is_some_and(|u| now >= u) {
                return None;
            }
            let wake = match (self.deadline, until) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            state = match wake {
                Some(w) => {
                    shared.parked.wait_timeout(state, w - now).unwrap_or_else(|e| e.into_inner()).0
                }
                None => shared.parked.wait(state).unwrap_or_else(|e| e.into_inner()),
            };
        }
    }

    /// Ends the run in progress with [`ExitReason::Stopped`], unless it
    /// already has an outcome: every vCPU stops, and [`wait`](Vcpus::wait)
    /// returns soon. Unlike a stop request, it leaves nothing behind for the
    /// next run.
    pub(crate) fn stop(&mut self) {
        assert!(self.in_run, "no run in progress");
        let mut state = self.shared.state();
        if state.outcome.is_none() {
            state.outcome = Some((Ok(ExitReason::Stopped), None));
        }
        drop(state);
        self.shared.stop_all();
    }
}

impl<V: VcpuLoop> Drop for Vcpus<V> {
    fn drop(&mut self) {
        // A run nobody finished (its `Run` forgotten) ends first: vCPU
        // threads see `quit` only between runs.
        if self.in_run {
            self.stop();
            self.wait(None);
        }
        self.shared.state().quit = true;
        self.shared.start.notify_all();
        for thread in self.threads.drain(..) {
            // A vCPU thread catches the panics of its vCPU; nothing else in
            // it panics, and a drop must not.
            let _ = thread.join();
        }
    }
}

fn vcpu_thread<V: VcpuLoop>(
    id: u32,
    vcpu: Arc<Mutex<V>>,
    shared: Arc<Shared>,
    ready: mpsc::SyncSender<Result<()>>,
) {
    let target = lock(&vcpu).kick_target();
    // SAFETY: the target is this thread's vCPU's, which `vcpu` keeps alive
    // until after the scope (declared later, dropped first).
    let scope = unsafe { VcpuThreadScope::enter(target) };
    let _scope = match scope {
        Ok(scope) => scope,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    // Dropped before `_scope`: the thread is never kicked once it
    // unregisters.
    let _registration = shared.stop.register(_scope.kicker());
    // Undone as the thread quits, below.
    lock(&vcpu).thread_started();
    let _ = ready.send(Ok(()));
    drop(ready);

    let mut seen = 0;
    loop {
        {
            let mut state = shared.state();
            while !state.quit && state.generation == seen && state.jobs[id as usize].is_none() {
                state = shared.start.wait(state).unwrap_or_else(|e| e.into_inner());
            }
            // Work between runs first: it is given only between runs, and its giver waits.
            if let Some(job) = state.jobs[id as usize].take() {
                drop(state);
                job();
                continue;
            }
            if state.quit {
                drop(state);
                lock(&vcpu).thread_ended();
                return;
            }
            seen = state.generation;
        }
        let ctl = RunControl { shared: &shared };
        let outcome = catch_unwind(AssertUnwindSafe(|| lock(&vcpu).run(&ctl)))
            .unwrap_or_else(|panic| Some(Err(panicked(id, &*panic))));
        if let Some(outcome) = outcome {
            shared.finish(outcome, id);
        }
        let mut state = shared.state();
        state.running -= 1;
        if state.running == 0 {
            shared.parked.notify_all();
        }
    }
}

fn panicked(id: u32, panic: &(dyn std::any::Any + Send)) -> Error {
    let message = panic
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("(no message)");
    Error::Hypervisor {
        op: "run a vCPU",
        source: std::io::Error::other(format!("vCPU {id} panicked: {message}")),
    }
}

/// How long a fake vCPU waits between checks (tests).
#[cfg(test)]
const FAKE_SPIN: Duration = Duration::from_micros(200);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stop::StopHandle;
    use std::sync::atomic::AtomicU64;

    /// What a fake vCPU does in a run.
    #[derive(Clone)]
    enum Script {
        /// Runs until stopped.
        Spin,
        /// Ends the run with this outcome after the delay.
        After(Duration, fn() -> Result<ExitReason>),
        /// Panics after the delay.
        Panic(Duration),
    }

    struct Fake {
        script: Arc<Mutex<Script>>,
        /// Runs this vCPU took part in.
        runs: Arc<AtomicU64>,
    }

    impl VcpuLoop for Fake {
        fn kick_target(&mut self) -> KickTarget {
            crate::backend::kick::fake_target()
        }

        fn run(&mut self, ctl: &RunControl<'_>) -> Option<Result<ExitReason>> {
            self.runs.fetch_add(1, Ordering::SeqCst);
            let start = Instant::now();
            let script = lock(&self.script).clone();
            loop {
                if let ControlFlow::Break(r) = ctl.check() {
                    return r;
                }
                match script {
                    Script::Spin => {}
                    Script::After(d, outcome) if start.elapsed() >= d => return Some(outcome()),
                    Script::Panic(d) if start.elapsed() >= d => panic!("fake vCPU fault"),
                    _ => {}
                }
                std::thread::sleep(FAKE_SPIN);
            }
        }
    }

    struct Machine {
        vcpus: Vcpus<Fake>,
        scripts: Vec<Arc<Mutex<Script>>>,
        runs: Vec<Arc<AtomicU64>>,
        stop: StopHandle,
    }

    fn machine(n: usize) -> Machine {
        let stop = Arc::new(StopShared::default());
        let scripts: Vec<_> = (0..n).map(|_| Arc::new(Mutex::new(Script::Spin))).collect();
        let runs: Vec<_> = (0..n).map(|_| Arc::new(AtomicU64::new(0))).collect();
        let fakes =
            (0..n).map(|i| Fake { script: scripts[i].clone(), runs: runs[i].clone() }).collect();
        Machine {
            vcpus: Vcpus::spawn(fakes, stop.clone()).unwrap(),
            scripts,
            runs,
            stop: StopHandle::new(stop),
        }
    }

    impl Machine {
        fn script(&self, vcpu: usize, script: Script) {
            *lock(&self.scripts[vcpu]) = script;
        }

        fn run_for(&mut self, t: Duration) -> Result<ExitReason> {
            self.vcpus.run(Some(Instant::now() + t))
        }

        fn runs(&self) -> Vec<u64> {
            self.runs.iter().map(|r| r.load(Ordering::SeqCst)).collect()
        }
    }

    const MS: Duration = Duration::from_millis(1);
    /// Long enough that a test never gets there unless the code hangs.
    const NEVER: Duration = Duration::from_secs(20);

    #[test]
    fn any_vcpu_ends_the_run_for_all() {
        for who in 0..4 {
            let mut m = machine(4);
            m.script(who, Script::After(5 * MS, || Ok(ExitReason::Poweroff)));
            let start = Instant::now();
            assert_eq!(m.run_for(NEVER).unwrap(), ExitReason::Poweroff);
            assert!(start.elapsed() < NEVER / 2);
            assert_eq!(m.vcpus.last_vcpu(), Some(who as u32));
            assert_eq!(m.runs(), [1, 1, 1, 1], "every vCPU ran once");
        }
    }

    #[test]
    fn the_first_outcome_wins_and_is_reported_once() {
        let mut m = machine(3);
        m.script(0, Script::After(40 * MS, || Ok(ExitReason::Reset)));
        m.script(2, Script::After(2 * MS, || Ok(ExitReason::Shutdown)));
        assert_eq!(m.run_for(NEVER).unwrap(), ExitReason::Shutdown);
        assert_eq!(m.vcpus.last_vcpu(), Some(2));
        // The later outcome was dropped, not kept for the next run.
        m.script(2, Script::Spin);
        m.script(0, Script::Spin);
        assert_eq!(m.run_for(20 * MS).unwrap(), ExitReason::TimedOut);
    }

    #[test]
    fn an_error_on_any_vcpu_stops_all_and_is_returned() {
        let mut m = machine(4);
        m.script(
            3,
            Script::After(3 * MS, || {
                Err(Error::Hypervisor {
                    op: "run vCPU",
                    source: std::io::Error::from_raw_os_error(14),
                })
            }),
        );
        match m.run_for(NEVER) {
            Err(Error::Hypervisor { op: "run vCPU", .. }) => {}
            other => panic!("expected the vCPU's error, got {other:?}"),
        }
        assert_eq!(m.vcpus.last_vcpu(), Some(3));
        // The machine can run again.
        m.script(3, Script::After(MS, || Ok(ExitReason::Poweroff)));
        assert_eq!(m.run_for(NEVER).unwrap(), ExitReason::Poweroff);
    }

    #[test]
    fn a_panicking_vcpu_is_an_error_not_a_hang() {
        let mut m = machine(2);
        m.script(1, Script::Panic(2 * MS));
        match m.run_for(NEVER) {
            Err(e) => assert!(e.to_string().contains("vCPU 1 panicked"), "{e}"),
            other => panic!("expected an error, got {other:?}"),
        }
        // Its thread lives on, and the machine runs again.
        m.script(1, Script::After(MS, || Ok(ExitReason::Reset)));
        assert_eq!(m.run_for(NEVER).unwrap(), ExitReason::Reset);
        assert_eq!(m.runs(), [2, 2]);
    }

    #[test]
    fn a_timeout_stops_every_vcpu() {
        let mut m = machine(4);
        let start = Instant::now();
        assert_eq!(m.run_for(30 * MS).unwrap(), ExitReason::TimedOut);
        let elapsed = start.elapsed();
        assert!(elapsed >= 30 * MS && elapsed < NEVER / 2, "{elapsed:?}");
        assert_eq!(m.vcpus.last_vcpu(), None);
        assert_eq!(m.runs(), [1, 1, 1, 1]);
        // A deadline already past runs nothing.
        assert_eq!(m.run_for(Duration::ZERO).unwrap(), ExitReason::TimedOut);
        assert_eq!(m.runs(), [1, 1, 1, 1]);
    }

    #[test]
    fn a_stop_request_stops_every_vcpu() {
        let mut m = machine(4);
        let stop = m.stop.clone();
        let stopper = std::thread::spawn(move || {
            std::thread::sleep(10 * MS);
            stop.stop();
        });
        assert_eq!(m.vcpus.run(None).unwrap(), ExitReason::Stopped);
        stopper.join().unwrap();
        assert_eq!(m.runs(), [1, 1, 1, 1]);
        // The request was consumed once.
        assert_eq!(m.run_for(10 * MS).unwrap(), ExitReason::TimedOut);
    }

    #[test]
    fn a_stop_before_the_run_is_kept_and_consumed_once() {
        let mut m = machine(2);
        m.stop.stop();
        assert_eq!(m.vcpus.run(None).unwrap(), ExitReason::Stopped);
        assert_eq!(m.runs(), [0, 0], "no vCPU ran");
        assert_eq!(m.run_for(10 * MS).unwrap(), ExitReason::TimedOut);
    }

    /// A stop request that a vCPU consumed after another vCPU ended the run
    /// is not lost: the next run returns Stopped at once.
    #[test]
    fn a_stop_request_that_loses_the_race_is_kept() {
        let shared = Shared {
            state: Mutex::default(),
            start: Condvar::new(),
            parked: Condvar::new(),
            stopping: AtomicBool::new(false),
            stop: Arc::default(),
        };
        shared.finish(Ok(ExitReason::Poweroff), 0);
        assert!(!shared.stop.take_request());
        shared.finish(Ok(ExitReason::Stopped), 1);
        assert!(shared.stop.take_request(), "the request was put back");
        assert!(matches!(shared.state().outcome, Some((Ok(ExitReason::Poweroff), Some(0)))));
    }

    #[test]
    fn many_short_runs_never_hang_or_lose_an_outcome() {
        let mut m = machine(4);
        for i in 0..200 {
            let who = i % 4;
            for v in 0..4 {
                m.script(v, Script::Spin);
            }
            match i % 3 {
                0 => {
                    m.script(who, Script::After(Duration::ZERO, || Ok(ExitReason::Reset)));
                    assert_eq!(m.run_for(NEVER).unwrap(), ExitReason::Reset, "run {i}");
                    assert_eq!(m.vcpus.last_vcpu(), Some(who as u32));
                }
                1 => assert_eq!(m.run_for(MS).unwrap(), ExitReason::TimedOut, "run {i}"),
                _ => {
                    m.stop.stop();
                    assert_eq!(m.run_for(NEVER).unwrap(), ExitReason::Stopped, "run {i}");
                }
            }
        }
    }

    /// Waits that reach their limit return with the vCPUs still running:
    /// no vCPU stops and restarts (each runs once), and the run's outcome
    /// comes from the vCPU, not from the waits.
    #[test]
    fn waiting_in_steps_leaves_the_vcpus_running() {
        let mut m = machine(4);
        m.script(2, Script::After(60 * MS, || Ok(ExitReason::Poweroff)));
        assert!(m.vcpus.start(None).is_none());
        let mut waits = 0;
        let outcome = loop {
            match m.vcpus.wait(Some(Instant::now() + 2 * MS)) {
                Some(outcome) => break outcome,
                None => waits += 1,
            }
            assert!(waits < 10_000, "the run never ended");
        };
        assert_eq!(outcome.unwrap(), ExitReason::Poweroff);
        // At least one wait returned with the run going on. How many fit in
        // the 60 ms depends on scheduling (a loaded host stretches a 2 ms
        // wait well past 10 ms), so no more is asserted: a lower bound of 5
        // failed under load (board 13161).
        assert!(waits >= 1, "{waits} waits");
        assert_eq!(m.runs(), [1, 1, 1, 1], "no vCPU was stopped by a wait");
        assert_eq!(m.vcpus.last_vcpu(), Some(2));
        assert!(!m.vcpus.in_run());
    }

    /// The run's timeout holds when the caller waits in steps.
    #[test]
    fn a_timeout_holds_across_waits_in_steps() {
        let mut m = machine(2);
        let start = Instant::now();
        assert!(m.vcpus.start(Some(start + 30 * MS)).is_none());
        let outcome = loop {
            if let Some(outcome) = m.vcpus.wait(Some(Instant::now() + 7 * MS)) {
                break outcome;
            }
        };
        assert_eq!(outcome.unwrap(), ExitReason::TimedOut);
        let elapsed = start.elapsed();
        assert!(elapsed >= 30 * MS && elapsed < NEVER / 2, "{elapsed:?}");
        assert_eq!(m.runs(), [1, 1]);
    }

    /// `stop` ends the run as Stopped and leaves no request behind; a run
    /// that already had an outcome keeps it.
    #[test]
    fn stop_ends_the_run_and_leaves_nothing_behind() {
        let mut m = machine(3);
        assert!(m.vcpus.start(None).is_none());
        assert!(m.vcpus.wait(Some(Instant::now() + 5 * MS)).is_none());
        m.vcpus.stop();
        assert_eq!(m.vcpus.wait(None).unwrap().unwrap(), ExitReason::Stopped);
        assert_eq!(m.vcpus.last_vcpu(), None);
        assert_eq!(m.run_for(10 * MS).unwrap(), ExitReason::TimedOut);

        m.script(0, Script::After(Duration::ZERO, || Ok(ExitReason::Reset)));
        assert!(m.vcpus.start(None).is_none());
        // Wait for the reset to be recorded (the vCPUs then stop).
        let deadline = Instant::now() + NEVER;
        while m.vcpus.shared.state().outcome.is_none() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(MS);
        }
        m.vcpus.stop();
        assert_eq!(m.vcpus.wait(None).unwrap().unwrap(), ExitReason::Reset);
    }

    /// A machine dropped in the middle of a run stops it and joins its
    /// threads.
    #[test]
    fn dropping_during_a_run_stops_it() {
        let mut m = machine(4);
        assert!(m.vcpus.start(None).is_none());
        let shared = Arc::downgrade(&m.vcpus.shared);
        drop(m);
        assert!(shared.upgrade().is_none());
    }

    /// Jobs run on each vCPU's own thread, between runs, their answers in vCPU order; a job's
    /// error or panic is the call's error, and the threads run on after it.
    #[test]
    fn jobs_run_on_each_vcpus_own_thread() {
        type Job = Box<dyn FnOnce(&mut Fake) -> Result<String> + Send>;
        let name = || Ok(std::thread::current().name().unwrap_or_default().to_owned());
        let mut m = machine(3);
        let jobs: Vec<Job> = (0..3).map(|_| Box::new(move |_: &mut Fake| name()) as Job).collect();
        assert_eq!(m.vcpus.on_each(jobs).unwrap(), ["vcpu0", "vcpu1", "vcpu2"]);

        let jobs: Vec<Job> = vec![
            Box::new(move |_| name()),
            Box::new(|_| Err(Error::Snapshot("vCPU 1's".into()))),
            Box::new(|_| panic!("a job's fault")),
        ];
        let err = m.vcpus.on_each(jobs).unwrap_err().to_string();
        assert!(err.contains("vCPU 1's"), "{err}");
        let jobs: Vec<Job> =
            vec![Box::new(move |_| name()), Box::new(move |_| name()), Box::new(|_| panic!("x"))];
        let err = m.vcpus.on_each(jobs).unwrap_err().to_string();
        assert!(err.contains("vCPU 2 panicked: x"), "{err}");
        m.script(1, Script::After(MS, || Ok(ExitReason::Poweroff)));
        assert_eq!(m.run_for(NEVER).unwrap(), ExitReason::Poweroff);
        assert_eq!(m.runs(), [1, 1, 1]);
    }

    #[test]
    fn dropping_joins_every_thread() {
        let m = machine(8);
        let shared = Arc::downgrade(&m.vcpus.shared);
        drop(m);
        assert!(shared.upgrade().is_none(), "every thread released its state");
    }
}
