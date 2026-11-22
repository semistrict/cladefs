// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: a call's caller, and whether it still waits for the call's answer (board 5599).
//!
//! The extent daemon runs each call of its control API to its end, whether or not its caller waits
//! for the answer ([`detached`]): a call stopped at whatever await its caller gave up in would
//! leave what it began half done (a directory claimed, a hold let go of while its work goes on, a
//! fork's child made and never recorded). What should end with its caller asks for that by name:
//! a wait that changes nothing, which only its caller would read the end of, also waits for
//! [`gone`], and stops there.

use futures::FutureExt;
use futures::channel::oneshot;
use futures::future::Shared;
use std::future::Future;

tokio::task_local! {
    /// Of the call the task runs: completes once its caller gives up on its answer.
    static GONE: Shared<oneshot::Receiver<()>>;
}

/// Runs `work`, a call's, to its end, in a task of its own, its caller's giving up, which drops
/// what this returns, stopping nothing but what waits for [`gone`]: `Err` if the task ended
/// otherwise than by finishing `work`, as when the runtime shuts down; a panic of `work` is this
/// one's.
pub(crate) fn detached<F>(
    work: F,
) -> impl Future<Output = Result<F::Output, Unfinished>> + Send + 'static
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    // Held by what awaits the answer: dropped with it, which `gone` hears.
    let (waits, gone) = oneshot::channel::<()>();
    let task = tokio::spawn(GONE.scope(gone.shared(), work));
    async move {
        let answered = task.await;
        drop(waits);
        match answered {
            Ok(answer) => Ok(answer),
            #[cfg(not(madsim))]
            Err(err) if err.is_panic() => std::panic::resume_unwind(err.into_panic()),
            Err(err) => Err(Unfinished(err.to_string())),
        }
    }
}

/// A call's task that ended without its work's answer ([`detached`]).
#[derive(Debug, thiserror::Error)]
#[error("the call's task ended unfinished: {0}")]
pub(crate) struct Unfinished(String);

/// Completes once the caller of the call this runs in gives up on its answer ([`detached`]); run
/// in no call's task, as a VM process's requests, never.
pub(crate) async fn gone() {
    match GONE.try_with(Shared::clone) {
        // Its sender dropped, never sent on: what the answer's awaiter held.
        Ok(gone) => drop(gone.await),
        // Outside an answer's task: never gone, as no awaiter's drop can say.
        Err(_) => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A call whose caller gives up runs to its end, hearing that it was given up on; one whose
    /// caller waits hears nothing of the kind.
    #[tokio::test]
    async fn calls_given_up_on_run_to_their_end() {
        let (ended, end) = oneshot::channel();
        let call = detached(async move {
            gone().await;
            ended.send("heard its caller gone").unwrap();
        });
        drop(call);
        assert_eq!(end.await, Ok("heard its caller gone"));

        let (open, opened) = oneshot::channel::<()>();
        let call = detached(async move {
            tokio::select! {
                () = gone() => "heard its caller gone",
                _ = opened => "answered",
            }
        });
        open.send(()).unwrap();
        assert_eq!(call.await.unwrap(), "answered");
    }

    /// Outside a call, nothing gives up.
    #[test]
    fn nothing_is_gone_outside_a_call() {
        assert_eq!(gone().now_or_never(), None);
    }

    /// A call's panic is its awaiter's.
    #[tokio::test]
    #[should_panic(expected = "the call's own")]
    async fn panics_of_calls_are_their_callers() {
        let _ = detached(async { panic!("the call's own") }).await;
    }
}
