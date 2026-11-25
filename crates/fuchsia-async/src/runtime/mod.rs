// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Fuchsia's `src/lib/fuchsia-async/src/runtime/mod.rs`, for the host: the portable runtime, without
// Fuchsia's; and in cladefs's guest kernel (target_os = "none"), a runtime of its own (`guest`).

#[cfg(not(target_os = "none"))]
mod portable;
#[cfg(not(target_os = "none"))]
use self::portable as implementation;
#[cfg(target_os = "none")]
mod guest;
#[cfg(target_os = "none")]
use self::guest as implementation;

// Exports common to all target os.
pub use implementation::executor::{
    LocalExecutor, LocalExecutorBuilder, MonotonicDuration, MonotonicInstant, SendExecutor,
    SendExecutorBuilder, SpawnableFuture, TestExecutor, TestExecutorBuilder,
};
pub use implementation::task::{JoinHandle, Task, unblock, yield_now};
pub use implementation::timer::Timer;

use futures::prelude::*;
use pin_project_lite::pin_project;
use std::pin::Pin;
use std::task::{Context, Poll};

/// The time when a Timer should wakeup.
pub trait WakeupTime {
    /// Create a timer based on this time.
    ///
    /// This is allowed to be inaccurate, but the inaccuracy must make the wakeup time later,
    /// never earlier.
    fn into_timer(self) -> Timer;
}

impl WakeupTime for std::time::Duration {
    fn into_timer(self) -> Timer {
        Timer::from(self)
    }
}

/// A trait which allows futures to be easily wrapped in a timeout.
pub trait TimeoutExt: Future + Sized {
    /// Wraps the future in a timeout, calling `on_timeout` to produce a result
    /// when the timeout occurs.
    fn on_timeout<WT, OT>(self, time: WT, on_timeout: OT) -> OnTimeout<Self, OT>
    where
        WT: WakeupTime,
        OT: FnOnce() -> Self::Output,
    {
        OnTimeout { timer: time.into_timer(), future: self, on_timeout: Some(on_timeout) }
    }
}

impl<F: Future + Sized> TimeoutExt for F {}

pin_project! {
    /// A wrapper for a future which will complete with a provided closure when a timeout occurs.
    #[derive(Debug)]
    #[must_use = "futures do nothing unless polled"]
    pub struct OnTimeout<F, OT> {
        #[pin]
        timer: Timer,
        #[pin]
        future: F,
        on_timeout: Option<OT>,
    }
}

impl<F: Future, OT> Future for OnTimeout<F, OT>
where
    OT: FnOnce() -> F::Output,
{
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        if let Poll::Ready(item) = this.future.poll(cx) {
            return Poll::Ready(item);
        }
        if let Poll::Ready(()) = this.timer.poll(cx) {
            let ot = this.on_timeout.take().expect("polled withtimeout after completion");
            let item = (ot)();
            return Poll::Ready(item);
        }
        Poll::Pending
    }
}

#[cfg(test)]
mod task_tests {

    use super::*;
    use futures::channel::oneshot;

    fn run(f: impl Send + 'static + Future<Output = ()>) {
        const TEST_THREADS: u8 = 2;
        SendExecutorBuilder::new().num_threads(TEST_THREADS).build().run(f)
    }

    #[test]
    fn can_detach() {
        run(async move {
            let (tx_started, rx_started) = oneshot::channel();
            let (tx_continue, rx_continue) = oneshot::channel();
            let (tx_done, rx_done) = oneshot::channel();
            {
                // spawn a task and detach it
                // the task will wait for a signal, signal it received it, and then wait for another
                Task::spawn(async move {
                    tx_started.send(()).unwrap();
                    rx_continue.await.unwrap();
                    tx_done.send(()).unwrap();
                })
                .detach();
            }
            // task is detached, have a short conversation with it
            rx_started.await.unwrap();
            tx_continue.send(()).unwrap();
            rx_done.await.unwrap();
        });
    }

    #[test]
    fn can_join() {
        // can we spawn, then join a task
        run(async move {
            assert_eq!(42, Task::spawn(async move { 42u8 }).await);
        })
    }

    #[test]
    fn can_join_unblock() {
        // can we poll a blocked task
        run(async move {
            assert_eq!(42, unblock(|| 42u8).await);
        })
    }

    #[test]
    fn can_join_unblock_local() {
        // can we poll a blocked task in a local executor
        LocalExecutorBuilder::new().build().run_singlethreaded(async move {
            assert_eq!(42, unblock(|| 42u8).await);
        });
    }

    #[test]
    #[should_panic]
    fn unblock_fn_panics() {
        run(async move {
            unblock(|| panic!("bad")).await;
        })
    }

    #[test]
    fn can_join_local() {
        // can we spawn, then join a task locally
        LocalExecutorBuilder::new().build().run_singlethreaded(async move {
            assert_eq!(42, Task::local(async move { 42u8 }).await);
        })
    }

    #[test]
    fn can_cancel() {
        run(async move {
            let (_tx_start, rx_start) = oneshot::channel::<()>();
            let (tx_done, rx_done) = oneshot::channel();
            // Start and immediately cancel the task (by dropping it).
            drop(Task::spawn(async move {
                rx_start.await.unwrap();
                tx_done.send(()).unwrap();
            }));
            // we should see an error on receive
            rx_done.await.expect_err("done should not be sent");
        })
    }
}

#[cfg(test)]
mod timer_tests {
    use super::*;
    use futures::future::Either;
    use std::pin::pin;

    #[test]
    fn shorter_fires_first_instant() {
        use std::time::{Duration, Instant};
        let mut exec = LocalExecutorBuilder::new().build();
        let now = Instant::now();
        let shorter = pin!(Timer::new(now + Duration::from_millis(100)));
        let longer = pin!(Timer::new(now + Duration::from_secs(1)));
        match exec.run_singlethreaded(future::select(shorter, longer)) {
            Either::Left((_, _)) => {}
            Either::Right((_, _)) => panic!("wrong timer fired"),
        }
    }
}
